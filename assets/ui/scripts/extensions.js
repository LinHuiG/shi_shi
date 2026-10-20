let sourceRequest=0,promptRequest=0;
let sourceCatalog=null,editingSource=null,sourceCandidates=[],promptHistory=null;
let pluginInstalling=false,sourceFormToken=0,sourceDiscoveryToken=0;
function updatePluginInstallUi(){
  document.querySelectorAll('[data-extension="install-plugin"]').forEach(b=>{b.disabled=pluginInstalling;});
  const hint=$('#plugin-drop-hint');if(hint)hint.textContent=pluginInstalling?'正在安装…':'＋ 安装插件';
}
async function installPlugin(send){
  if(pluginInstalling){toast('正在安装插件，请稍候');return;}
  pluginInstalling=true;updatePluginInstallUi();
  try{const installed=await send();pluginCatalogRevision++;const status=installed.install_status||'installed';if(page==='sources'){await renderSources();if(status==='installed'&&page==='sources'&&$('#overlay').hidden&&!sourceCatalog.sources.some(s=>s.plugin_id===installed.id))sourceForm({plugin_id:installed.id,id:'',name:installed.name,config:{},enabled:true,updated:0});}toast(status==='unchanged'?'此插件已是相同版本和内容，无需重复安装':status==='updated'?'插件已更新，来源配置和读取进度已保留':'插件已安装');}
  finally{pluginInstalling=false;updatePluginInstallUi();}
}
function installPluginPath(path){return installPlugin(()=>rpc('plugin_install',{path}));}
async function installPluginFile(file){
 if(!file||!file.name.toLowerCase().endsWith('.zip'))throw Error('请拖入插件 ZIP 包');
 if(file.size>32*1024*1024)throw Error('拖入的 ZIP 超过32 MiB，请点击安装区域选择文件');
 return installPlugin(async()=>{const base64=await new Promise((resolve,reject)=>{const reader=new FileReader();reader.onload=()=>resolve(String(reader.result).split(',')[1]);reader.onerror=()=>reject(Error('无法读取拖入的插件包，请重试'));reader.readAsDataURL(file);});return rpc('plugin_upload',{name:file.name,base64});});
}
function acceptsPluginDrop(){return page==='sources'&&$('#overlay').hidden&&!pluginInstalling;}
for(const type of ['dragenter','dragover'])document.addEventListener(type,e=>{
 if(!Array.from(e.dataTransfer?.types||[]).includes('Files'))return;
 e.preventDefault();const accept=acceptsPluginDrop();e.dataTransfer.dropEffect=accept?'copy':'none';$('#view').classList.toggle('plugin-drag-over',accept);
});
document.addEventListener('dragleave',e=>{if(!e.relatedTarget||!document.contains(e.relatedTarget))$('#view').classList.remove('plugin-drag-over');});
document.addEventListener('drop',async e=>{
 if(!Array.from(e.dataTransfer?.types||[]).includes('Files'))return;
 e.preventDefault();$('#view').classList.remove('plugin-drag-over');
 if(!acceptsPluginDrop()){toast(pluginInstalling?'正在安装插件，请稍候':page!=='sources'?'请在“消息来源”页面拖入插件 ZIP 包':'请先完成或关闭当前配置');return;}
 const files=Array.from(e.dataTransfer.files);if(files.length!==1){toast('请一次拖入一个插件 ZIP 包');return;}
 try{await installPluginFile(files[0]);}catch(error){toast(error.message);}
});
const promptLabels={classify:'消息分析',format:'自然语言新建',learning:'优化建议',report:'报表汇总',handling:'AI一键处置',prompt_review:'提示词建议',archive:'归档与记忆'};
async function renderSources(){
  const request=++sourceRequest,epoch=renderEpoch,retainContent=$('#view').dataset.contentPage==='sources'&&!!sourceCatalog;
  if($('#view').dataset.contentPage!=='sources'){setHtml('#view','<div class="page"><div class="box">读取插件…</div></div>');$('#view').dataset.contentPage='sources';}
  try{const catalog=await rpc('plugins');if(page!=='sources'||epoch!==renderEpoch||request!==sourceRequest)return;sourceCatalog=catalog;
    setHtml('#view',`<div class="page sources-page"><header class="source-toolbar"><h2>插件</h2><button type="button" class="plugin-dropzone" data-extension="install-plugin"><span id="plugin-drop-hint">＋ 安装插件</span></button>${help('点击选择或拖入插件 ZIP；仅安装可信插件。')}</header><div class="plugin-strip">${catalog.plugins.map(p=>`<article class="plugin-tile"><div class="row"><b>${esc(p.name)}</b><span class="tiny">${esc(p.version)}</span></div><div class="plugin-actions"><button role="switch" aria-checked="${p.enabled!==false}" class="switch ${p.enabled!==false?'on':''}" data-source-plugin-toggle="${esc(p.id)}">${p.enabled!==false?'已开启':'已停用'}</button><button class="quiet" data-source-plugin-uninstall="${esc(p.id)}">卸载</button></div><button data-source-add="${esc(p.id)}" ${p.enabled===false?'disabled':''}>＋ 添加来源</button></article>`).join('')||'<p class="plugin-empty">尚未安装插件</p>'}</div>${catalog.errors.map(e=>`<div class="error-box">${esc(e)}</div>`).join('')}<h2 class="source-section-title">接入源 <span class="tiny">${catalog.sources.length}</span></h2><div class="source-grid">${catalog.sources.map(s=>`<article class="source-card"><div class="row"><h3>${esc(s.name)}</h3><span class="badge ${s.enabled?'green':''}">${s.enabled?'已启用':'已停用'}</span></div><span class="muted">${esc(catalog.plugins.find(p=>p.id===s.plugin_id)?.name||'插件不可用')}</span><p class="tiny">最近读取 ${esc(date(s.last_scan))}</p>${s.last_error?`<div class="error-box">${esc(s.last_error)}</div>`:''}<div class="source-card-actions"><button data-source-edit="${esc(s.id)}">配置</button><button data-source-range="${esc(s.id)}">补读</button><button class="quiet" data-source-remove="${esc(s.id)}">删除</button></div></article>`).join('')||'<p class="muted">选择上方插件添加来源。</p>'}</div></div>`);
    updatePluginInstallUi();
  }catch(e){if(page==='sources'&&epoch===renderEpoch&&request===sourceRequest){renderedKey='';if(retainContent){toast(e.message);return;}setHtml('#view',`<div class="page"><div class="error-box">${esc(e.message)}</div></div>`);}}
}
function sourceForm(source){
  editingSource=structuredClone(source);sourceCandidates=[];sourceFormToken++;
  const plugin=sourceCatalog.plugins.find(p=>p.id===source.plugin_id);if(!plugin)throw new Error('插件不可用，请先安装完整插件包');
  setModal(`<h2>${esc(plugin.name)} · ${source.id?'配置来源':'添加来源'}</h2>${help("填写文件和目录，或自动查找；来源配置中请勿填写密钥。")}<div class="form-group"><label for="source-name">来源名称</label><input id="source-name" maxlength="100" value="${esc(source.name||plugin.name)}"></div>${plugin.config_fields.map(f=>`<div class="form-group"><label for="source-field-${esc(f.key)}">${esc(f.label)}${f.required?' *':''}</label><div class="field-row"><input id="source-field-${esc(f.key)}" type="${f.type==='password'?'password':'text'}" value="${esc(source.config?.[f.key]||'')}" spellcheck="false">${['file','directory'].includes(f.type)?`<button data-source-pick="${esc(f.key)}">选择…</button>`:''}</div>${f.description?help(f.description):""}</div>`).join('')}<button data-extension="source-discover">重新查找</button><div id="source-candidates" class="helper" style="margin:12px 0"></div><label class="check"><input id="source-enabled" type="checkbox" ${source.enabled?'checked':''}>校验后启用此来源</label><div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">取消</button><button class="primary" data-extension="source-save">保存配置</button></div>`);
  if(plugin.capabilities.includes('discover'))void discoverSource();
}

function sourceConfig(){const plugin=sourceCatalog.plugins.find(p=>p.id===editingSource.plugin_id);return Object.fromEntries(plugin.config_fields.map(f=>[f.key,$('#source-field-'+f.key).value.trim()]));}
function sourceRangeForm(id){setModal(`<h2>补读消息</h2><p class="subtitle">补读历史消息并分析。</p>${help("单次最多 31 天，不改变定时扫描进度。")}<div class="form-group"><label>开始</label><input id="range-from" type="datetime-local" value="${localDue(Date.now()-86400000)}"></div><div class="form-group"><label>结束</label><input id="range-until" type="datetime-local" value="${localDue(Date.now()-60000)}"></div><div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">取消</button><button class="primary" data-extension="source-range" data-id="${esc(id)}">读取并分析</button></div>`);}
function changedText(oldText,newText){
  let prefix=0;while(prefix<oldText.length&&prefix<newText.length&&oldText[prefix]===newText[prefix])prefix++;
  let suffix=0;while(suffix<oldText.length-prefix&&suffix<newText.length-prefix&&oldText.at(-suffix-1)===newText.at(-suffix-1))suffix++;
  return [oldText,newText].map(text=>esc(text.slice(0,prefix))+'<mark>'+esc(text.slice(prefix,suffix?text.length-suffix:text.length))+'</mark>'+esc(suffix?text.slice(-suffix):''));
}
function promptPreview(id){
  const r=promptHistory.reviews.find(r=>r.id===id),diff=changedText(r.base_prompt,r.proposed_prompt||r.base_prompt);
  const stale=promptHistory.current.classify!==r.base_version;
  setModal(`<h2>消息分析提示词建议</h2><p style="white-space:pre-wrap">${esc(r.reason)}</p><div class="helper">日志依据：${esc(r.evidence_ids.join('、')||'无需修改')} · ${esc(r.day)}</div>${stale&&r.status==='ready'?'<div class="error-box">当前提示词已变化，请重新生成建议后采用。</div>':''}<div class="prompt-comparison"><section><h3>归档时的版本</h3><pre>${diff[0]}</pre></section><section><h3>建议的新版本</h3><pre>${diff[1]}</pre></section></div><div class="helper">高亮显示首尾相同内容之间的变更范围。采用后仅影响后续分析。</div><div class="dialog-foot"><button data-action="close">关闭</button>${r.status==='ready'?`<button class="primary" data-prompt-adopt="${r.id}" ${stale?'disabled':''}>采用此提示词</button>`:''}</div>`);
  $('#dialog').classList.add('wide-dialog');
}
function applySourceCandidate(index){
 const candidate=sourceCandidates[index];if(!candidate)return;
 for(const f of sourceCatalog.plugins.find(p=>p.id===editingSource.plugin_id).config_fields){const input=$('#source-field-'+f.key);if(input)input.value=typeof candidate.config?.[f.key]==='string'?candidate.config[f.key]:'';}
 const select=$('#source-candidate');if(select)select.value=String(index);
}
async function discoverSource(){
 const host=$('#source-candidates'),button=$('[data-extension="source-discover"]'),save=$('[data-extension="source-save"]');if(!host||button.disabled)return;
 const form=sourceFormToken,request=++sourceDiscoveryToken,config=sourceConfig(),plugin_id=editingSource.plugin_id;
 const current=()=>form===sourceFormToken&&request===sourceDiscoveryToken&&host.isConnected&&!$('#overlay').hidden;
 button.disabled=true;save.disabled=true;host.textContent='正在自动查找程序、文件和目录…';
 try{const result=await rpc('source_discover',{plugin_id,config});if(!current())return;sourceCandidates=result.candidates||[];
  const changed=JSON.stringify(sourceConfig())!==JSON.stringify(config);
  const match=sourceCandidates.findIndex(c=>Object.entries(config).filter(([,v])=>v).every(([key,v])=>c.config?.[key]===v));
  const selected=changed?-1:sourceCandidates.length===1?0:Object.values(config).some(Boolean)?match:-1;
  host.innerHTML=`${sourceCandidates.length?`<label>${sourceCandidates.length===1?'找到 1 个候选':`找到 ${sourceCandidates.length} 个候选，请核对选择`}</label><select id="source-candidate"><option value="">选择完整候选配置…</option>${sourceCandidates.map((c,i)=>`<option value="${i}">${esc(c.label)}</option>`).join('')}</select>`:'未找到可用配置。可填写路径或目录后重新查找。'}<div>${(result.warnings||[]).map(esc).join('<br>')}</div><div>${changed?'查找期间你修改了配置，已保留输入；可选择候选整套替换。':selected>=0?'已自动选中候选并填入完整配置，请核对后保存。':''}</div>`;
  if(selected>=0)applySourceCandidate(selected);
 }catch(error){if(current())host.textContent='自动查找失败：'+error.message+'；可手动填写或重新查找。';}
 finally{if(current()){button.disabled=false;save.disabled=false;}}
}
document.addEventListener('change',e=>{if(e.target.id==='source-candidate'&&e.target.value!=='')applySourceCandidate(Number(e.target.value));});

document.addEventListener('click',async e=>{
  const b=e.target.closest('button');if(!b||!(b.dataset.extension||Object.keys(b.dataset).some(k=>k.startsWith('source')||k.startsWith('prompt'))))return;
  try{
    if(b.dataset.sourcePluginToggle){const p=sourceCatalog.plugins.find(p=>p.id===b.dataset.sourcePluginToggle);await rpc('plugin_toggle',{id:p.id,enabled:p.enabled===false});pluginCatalogRevision++;await refreshState(true);return;}
    if(b.dataset.sourcePluginUninstall){const p=sourceCatalog.plugins.find(p=>p.id===b.dataset.sourcePluginUninstall);setModal(`<h2>卸载 ${esc(p.name)}</h2><p class="subtitle">卸载插件文件并停用其消息来源，保留来源配置、历史消息和事项。以后可重新安装插件包。</p><div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">取消</button><button data-extension="uninstall-plugin" data-id="${esc(p.id)}">确认卸载</button></div>`);return;}
    if(b.dataset.sourceAdd){sourceForm({plugin_id:b.dataset.sourceAdd,id:'',config:{},enabled:true,updated:0});return;}
    if(b.dataset.sourceEdit){sourceForm(sourceCatalog.sources.find(s=>s.id===b.dataset.sourceEdit));return;}
    if(b.dataset.sourceRange){sourceRangeForm(b.dataset.sourceRange);return;}
    if(b.dataset.promptPreview){promptPreview(b.dataset.promptPreview);return;}
    if(b.dataset.extension==='source-discover'){await discoverSource();return;}
    b.disabled=true;
    if(b.dataset.sourcePick){const f=sourceCatalog.plugins.find(p=>p.id===editingSource.plugin_id).config_fields.find(f=>f.key===b.dataset.sourcePick);const value=await rpc('pick',{kind:f.label,type:f.type});if(value)$('#source-field-'+f.key).value=value;return;}
    if(b.dataset.sourceRemove){if(!confirm('移除此来源？已有事项和消息证据会保留。'))return;const source=sourceCatalog.sources.find(s=>s.id===b.dataset.sourceRemove);await rpc('source_remove',{id:source.id,updated:source.updated});await refreshState(true);return;}
    if(b.dataset.promptRetry){await rpc('prompt_retry',{id:b.dataset.promptRetry});await renderPromptHistory();toast('重新评估完成，结果已保存');return;}
    if(b.dataset.promptAdopt){await rpc('prompt_adopt',{id:b.dataset.promptAdopt});closeModal();await refreshState(true);toast('已采用新提示词，原版本已存档');return;}
    if(b.dataset.promptRollback){const v=promptHistory.versions.find(v=>v.id===b.dataset.promptRollback);await rpc('prompt_rollback',{id:v.id,expected:promptHistory.current[v.target]});await refreshState(true);toast('已回滚，本次回滚也已存档');return;}
    switch(b.dataset.extension){
      case 'uninstall-plugin':{await rpc('plugin_uninstall',{id:b.dataset.id});pluginCatalogRevision++;closeModal();await refreshState(true);toast('插件已卸载，历史消息和事项已保留');break;}
      case 'install-plugin':{if(pluginInstalling)return;const path=await rpc('pick',{kind:'插件 ZIP 包或 plugin.json',type:'plugin'});if(path)await installPluginPath(path);break;}

      case 'source-save':{b.dataset.pending='true';const source={...editingSource,name:$('#source-name').value.trim(),config:sourceConfig(),enabled:$('#source-enabled').checked};await rpc('source_save',{source});delete b.dataset.pending;closeModal();await refreshState(true);toast(source.enabled?'配置已校验并启用':'配置已保存，暂未启用');break;}
      case 'source-range':{b.dataset.pending='true';const result=await rpc('source_range',{id:b.dataset.id,since:new Date($('#range-from').value+':00+08:00').getTime(),until:new Date($('#range-until').value+':00+08:00').getTime()});delete b.dataset.pending;closeModal();await refreshState(true);toast(`已补读 ${result.rows} 条消息`);break;}
    }
  }catch(error){const el=$('#modal-error');if(el)el.textContent=error.message;toast(error.message);}finally{b.disabled=false;delete b.dataset.pending;}
});
