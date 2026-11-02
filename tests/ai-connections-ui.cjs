const fs=require('fs'),path=require('path'),assert=require('assert'),{withBrowser}=require('./ui-driver.cjs');
const root=path.resolve(__dirname,'..'),out=fs.mkdtempSync(path.join(require('os').tmpdir(),'shishi-ai-connections-ui-'));
const model=id=>({model:id,displayName:id,supportedReasoningEfforts:[{reasoningEffort:'low'},{reasoningEffort:'high'}]});
const fixture={config:{codex:'original.exe',codex_models:[model('original-model')],ai_connections:[],model:'original-model',reasoning_effort:'low',leaders:[],enabled:false,auto_ai:false,interval_minutes:5,analysis_interval_minutes:30,analysis_lookback_hours:24,daily_calls:40,max_input_chars:60000,message_retention_days:90,work_start:'09:00',work_end:'18:00'},tasks:[],schedules:[],messages:[],usage:{},activity:[],audit:[],reports:[],suggestions:[],chat_sources:[],handling:[]};
const mock=`window.fixture=${JSON.stringify(fixture)};window.requests=[];window.ipc={postMessage(raw){const {id,method,params:p}=JSON.parse(raw);requests.push({method,p});const file=p?.connection?.executable;setTimeout(()=>{let data=true;
if(method==='state')data=fixture;
if(method==='detect')data=p.driver==='dsh'?{candidates:window.detectedDsh||[{path:'D:/Fixture Harness/resources/runtime/cli/bin/dsh.cmd',label:'Harness',version:'1.0'}]}:{codex:window.detectedCodex||'second.exe'};
if(method==='ai_connection_status')data=file==='uninitialized.cmd'&&!window.acpInitialized?{ready:false,can_initialize_acp:true,dsh_home:'D:/Fixture Home',status:'ACP 环境尚未初始化，初始化后点击刷新模型'}:{ready:true};
if(method==='dsh_workspace_plugin'){if(p.install){if(window.failPluginInstall){receive({id,ok:false,error:'模拟插件写入失败'});return;}window.pluginInstalled=true;}data={installed:window.pluginInstalled===true};}
if(method==='dsh_initialize_acp'){if(window.failInitialize){window.receive({id,ok:false,error:'模拟初始化失败'});return;}window.acpInitialized=true;data={ready:true};}
if(method==='models'){if(file==='uninitialized.cmd'&&!window.acpInitialized){window.receive({id,ok:false,error:'Harness 未就绪：ACP 环境尚未初始化\\n$env:DSH_HOME=fixture; 初始化后刷新模型'});return;}if(file==='broken.exe'||p.connection.driver==='api'){window.receive({id,ok:false,error:'模拟检测失败'});return;}const name=file==='slow.exe'?'stale-model':file?.includes('dsh.cmd')?'dsh-model':file==='original.exe'?'original-model':'second-model';data={models:[{model:name,displayName:name,supportedReasoningEfforts:[{reasoningEffort:'low'},{reasoningEffort:'high'}]}]};}
if(method==='connection_save'){if(window.failSave){window.receive({id,ok:false,error:'模拟保存失败'});return;}const c=p.connection;if(!c.id){fixture.config.codex=c.executable;fixture.config.codex_models=c.models;}else {const i=fixture.config.ai_connections.findIndex(v=>v.id===c.id);if(i<0)fixture.config.ai_connections.push(c);else fixture.config.ai_connections[i]=c;}data={connection:c,config:fixture.config};}
if(method==='connection_delete'){fixture.config.ai_connections=fixture.config.ai_connections.filter(c=>c.id!==p.id);data={config:fixture.config};}
if(method==='settings_patch'){const profiles={...fixture.config.model_profiles,...p.model_profiles};Object.assign(fixture.config,p);if(p.model_profiles)fixture.config.model_profiles=profiles;data={config:fixture.config};}if(method==='config')fixture.config=p;if(method==='scan_control'){fixture.config.enabled=p.enabled;data={enabled:p.enabled};}
if(method==='pick')data='second.exe';window.receive({id,ok:true,data});},method==='dsh_workspace_plugin'&&p.install?100:file==='slow.exe'?300:10)}};`;
const html=require('../scripts/frontend.cjs').build().replace('<script>','<script>'+mock+'\n');
withBrowser(async(browser)=>{const page=await browser.newPage({viewport:{width:1280,height:1000}}),errors=[];page.on('pageerror',e=>errors.push(e.message));await page.setContent(html);await page.waitForFunction(()=>typeof state!=='undefined'&&state?.config);await page.evaluate(()=>{page='settings';render();});
assert.equal(await page.locator('[data-ai-connection]').count(),1);assert.equal(await page.locator('[data-ai-path]').count(),0);assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='models').length),0,'settings uses saved catalog without a network call');
assert.equal(await page.locator('#cfg-model').inputValue(),'original-model');assert.equal(await page.locator('#cfg-handling-connection option:checked').innerText(),'默认 Codex');assert.equal(await page.locator('[data-model-custom]').count(),0);
// Merely opening/closing a saved editor never discovers models or prompts.
let dialogs=0,discard=true;page.on('dialog',dialog=>{dialogs++;return discard?dialog.accept():dialog.dismiss();});
await page.locator('[data-ai-edit=""]').click();assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='models').length),0);
await page.locator('[data-ai-name]').focus();await page.locator('[data-ai-path]').focus();assert.equal(await page.locator('#connection-form').count(),1,'internal focus preserved');
await page.locator('#page-title').click();await page.locator('#connection-form').waitFor({state:'detached'});
await page.locator('[data-ai-edit=""]').click();await page.locator('[data-ai-name]').fill('changed');assert(await page.evaluate(()=>connectionDirty()));await page.locator('[data-ai-name]').fill('默认 Codex');assert(!(await page.evaluate(()=>connectionDirty())));await page.locator('[data-ai-cancel]').click();
// Every part of the editor is inside the focus boundary, including non-focusable blank space.
for(const adding of [false,true])for(const dirty of [false,true]){
 await page.locator(adding?'[data-ai-add]':'[data-ai-edit=""]').click();if(dirty)await page.locator('[data-ai-name]').fill('blank-space draft');const beforeBlank=dialogs;
 for(const target of ['.catalog-heading b','#connection-models','.catalog-heading']){await page.locator('[data-ai-name]').focus();await page.locator(target).click({position:{x:5,y:5}});await page.waitForTimeout(30);assert.equal(dialogs,beforeBlank,'internal blank click cannot ask to discard');assert.equal(await page.locator('#connection-form').count(),1);}
 await page.locator('[data-ai-cancel]').click();
}
// Removing an internal control must not be mistaken for leaving the editor.
await page.locator('[data-ai-edit=""]').click();const modelDialogs=dialogs;await page.locator('[data-ai-model-remove="0"]').click();await page.waitForTimeout(30);assert.equal(dialogs,modelDialogs);assert.equal(await page.locator('#connection-form').count(),1);await page.locator('[data-ai-cancel]').click();
await page.locator('[data-ai-edit=""]').click();await page.locator('[data-ai-model-id]').fill('pending-model');assert(await page.evaluate(()=>connectionDirty()));await page.locator('[data-ai-cancel]').click();
// Passive blur keeps dirty drafts without a prompt; explicit navigation still asks.
await page.locator('[data-ai-edit=""]').click();await page.locator('[data-ai-name]').fill('draft to keep');discard=false;const firstDialog=dialogs;
await page.locator('#page-title').click();assert.equal(dialogs,firstDialog);assert.equal(await page.locator('[data-ai-name]').inputValue(),'draft to keep');
await page.locator('[data-page="tasks"]').click();assert.equal(dialogs,firstDialog+1);assert.equal(await page.evaluate(()=>page),'settings');
await page.locator('[data-ai-save]').focus();await page.keyboard.press('Tab');await page.waitForTimeout(30);assert.equal(dialogs,firstDialog+1);assert.equal(await page.locator('[data-ai-name]').inputValue(),'draft to keep');
await page.keyboard.press('Control+1');assert.equal(await page.evaluate(()=>page),'settings');await page.keyboard.press('Control+k');assert.equal(await page.locator('#palette').count(),0,'palette cannot bypass rejected editor leave');assert.equal(await page.locator('[data-ai-name]').inputValue(),'draft to keep');
const beforeCancel=dialogs;await page.locator('[data-ai-cancel]').click();assert.equal(dialogs,beforeCancel,'cancel discards directly');assert.equal(await page.locator('#connection-form').count(),0);discard=true;
await page.locator('[data-ai-edit=""]').click();assert.equal(await page.locator('[data-ai-name]').inputValue(),'默认 Codex');await page.locator('[data-ai-name]').fill('keep on blur');const beforeBlur=dialogs;await page.locator('#page-title').click();assert.equal(dialogs,beforeBlur);assert.equal(await page.locator('#connection-form').count(),1);await page.locator('[data-ai-name]').fill('默认 Codex');await page.locator('#page-title').click();await page.locator('#connection-form').waitFor({state:'detached'});
// Add follows the same rules and never saves a discarded draft.
await page.locator('[data-ai-add]').click();await page.locator('[data-ai-name]').fill('new draft');discard=false;await page.locator('#page-title').click();assert.equal(await page.locator('[data-ai-name]').inputValue(),'new draft');const newCancel=dialogs;await page.locator('[data-ai-cancel]').click();assert.equal(dialogs,newCancel);discard=true;
await page.locator('[data-ai-add]').click();await page.locator('[data-ai-name]').fill('new retained draft');const newBlur=dialogs;await page.locator('#page-title').click();assert.equal(dialogs,newBlur);assert.equal(await page.locator('#connection-form').count(),1);await page.locator('[data-ai-cancel]').click();assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='connection_save').length),0);
// Switching editors prompts once; unrelated settings still have their own protection.
await page.locator('[data-ai-edit=""]').click();await page.locator('[data-ai-name]').fill('switch draft');discard=false;const switchDialogs=dialogs;await page.locator('[data-ai-add]').click();assert.equal(dialogs,switchDialogs+1);assert.equal(await page.locator('[data-ai-name]').inputValue(),'switch draft');discard=true;await page.locator('[data-ai-add]').click();assert.equal(dialogs,switchDialogs+2);assert.equal(await page.locator('[data-ai-name]').inputValue(),'');await page.locator('[data-ai-cancel]').click();
// DSH readiness errors direct users to repair/retry instead of manual model entry.
await page.locator('[data-ai-add]').click();await page.locator('[data-ai-driver]').selectOption('dsh');await page.locator('[data-ai-path]').fill('uninitialized.cmd');await page.locator('[data-ai-models]').click();await page.waitForFunction(()=>document.querySelector('#connection-status').textContent.includes('ACP 环境尚未初始化'));assert(!(await page.locator('#connection-status').innerText()).includes('请手动添加或重试'));assert((await page.locator('#connection-status').innerText()).includes('刷新模型'));assert(await page.locator('[data-ai-initialize]').isVisible());
const initializations=await page.evaluate(()=>requests.filter(r=>r.method==='dsh_initialize_acp').length);discard=false;await page.locator('[data-ai-initialize]').click();assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='dsh_initialize_acp').length),initializations);assert.equal(await page.locator('#connection-form').count(),1);discard=true;
await page.evaluate(()=>window.failInitialize=true);await page.locator('[data-ai-initialize]').click();await page.waitForFunction(()=>document.querySelector('#connection-status').textContent.includes('ACP 初始化失败'));assert.equal(await page.locator('[data-ai-path]').inputValue(),'uninitialized.cmd');await page.evaluate(()=>window.failInitialize=false);
await page.locator('[data-ai-initialize]').click();await page.waitForFunction(()=>document.querySelector('#connection-models').textContent.includes('second-model'));assert(await page.locator('#connection-setup').isHidden());assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='dsh_initialize_acp').length),initializations+2);assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='connection_save').length),0,'initialization does not save the connection draft');await page.locator('[data-ai-cancel]').click();
// Settings normalizes numbers and reverted edits; top toggle saves only its field.
await page.locator('#cfg-budget').fill('41');assert(await page.evaluate(()=>settingsChanged()));await page.locator('#cfg-budget').fill('40');assert(!(await page.evaluate(()=>settingsChanged())));
await page.locator('#cfg-budget').fill('42');await page.locator('[data-action="toggle-scan"]').click();await page.waitForFunction(()=>requests.some(r=>r.method==='scan_control'));assert.equal(await page.locator('#cfg-budget').inputValue(),'42');await page.waitForFunction(()=>fixture.config.daily_calls===42);assert(!(await page.evaluate(()=>settingsChanged())));await page.locator('#cfg-budget').fill('40');await page.locator('#page-title').click();await page.waitForFunction(()=>fixture.config.daily_calls===40);
// A new editor is inert until detection is explicitly requested, then it auto-saves.
await page.locator('[data-ai-add]').click();await page.locator('[data-ai-cancel]').click();assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='connection_save').length),0);
await page.locator('[data-ai-add]').click();await page.locator('[data-ai-driver]').selectOption('dsh');await page.locator('[data-ai-detect]').click();await page.locator('#connection-form').waitFor({state:'detached'});assert.equal(await page.locator('[data-ai-connection]').count(),2);
const id=await page.evaluate(()=>fixture.config.ai_connections[0].id);
// Exercise the real plugin button and dispatcher, not the installer service directly.
await page.locator('[data-ai-edit="'+id+'"]').click();await page.waitForFunction(()=>!document.querySelector('[data-workspace-plugin-install]').disabled);
assert(await page.locator('[data-workspace-plugin-install]').isVisible());
assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='dsh_workspace_plugin'&&r.p.install).length),0);
await page.evaluate(()=>window.failPluginInstall=true);await page.locator('[data-workspace-plugin-install]').click();
assert(await page.locator('[data-workspace-plugin-install]').isDisabled());
await page.waitForFunction(()=>document.querySelector('[data-workspace-plugin-status]').textContent.includes('模拟插件写入失败'));
assert(!(await page.locator('[data-workspace-plugin-install]').isDisabled()));
await page.evaluate(()=>window.failPluginInstall=false);await page.locator('[data-workspace-plugin-install]').click();
await page.waitForFunction(()=>document.querySelector('[data-workspace-plugin-install]').hidden);
assert((await page.locator('[data-workspace-plugin-status]').innerText()).includes('重启 DSH'));
assert.deepEqual(await page.evaluate(()=>requests.filter(r=>r.method==='dsh_workspace_plugin'&&r.p.install).map(r=>[r.p.install,r.p.confirmed])),[[true,true],[true,true]]);
await page.locator('#connection-workspace-plugin').screenshot({path:path.join(out,'workspace-plugin-installed.png')});await page.locator('[data-ai-cancel]').click();
// Repeated detection of the same launcher cannot create a second connection.
await page.locator('[data-ai-add]').click();await page.locator('[data-ai-driver]').selectOption('dsh');await page.locator('[data-ai-detect]').click();await page.locator('#connection-form').waitFor({state:'detached'});assert.equal(await page.locator('[data-ai-connection]').count(),2);
await page.locator('#cfg-analysis-connection').selectOption(id);await page.locator('#cfg-model').selectOption(await page.locator('#cfg-model option').nth(1).getAttribute('value'));await page.locator('#cfg-effort').selectOption('high');
await page.locator('#page-title').click();await page.evaluate(()=>flushSettings());await page.waitForFunction(()=>fixture.config.model_profiles?.analysis.connection_id);assert.equal(await page.evaluate(()=>fixture.config.model_profiles.analysis.reasoning_effort),'high');
// Discovery is explicit and cannot overwrite an intervening field edit.
await page.locator('[data-ai-edit="'+id+'"]').click();const before=await page.evaluate(()=>requests.filter(r=>r.method==='models').length);await page.locator('[data-ai-path]').fill('slow.exe');await page.locator('[data-ai-models]').click();await page.locator('[data-ai-path]').fill('second.exe');await page.locator('[data-ai-models]').click();await page.waitForFunction(()=>document.querySelector('#connection-models').textContent.includes('second-model'));await page.waitForTimeout(350);assert(!(await page.locator('#connection-models').innerText()).includes('stale-model'));
await page.locator('[data-ai-name]').fill('kept draft');await page.evaluate(async()=>{window.draftNode=document.querySelector('[data-ai-name]');await refreshState(false);});assert(await page.evaluate(()=>draftNode.value==='kept draft'));
await page.evaluate(()=>window.failSave=true);await page.locator('[data-ai-save]').click();await page.waitForFunction(()=>document.querySelector('#connection-status').textContent.includes('保存失败'));assert.equal(await page.locator('[data-ai-name]').inputValue(),'kept draft');await page.evaluate(()=>window.failSave=false);await page.locator('[data-ai-cancel]').click();
// Duplicate display names retain opaque provider/model routes through save and refresh.
assert.deepEqual(await page.evaluate(()=>{
 const catalog=[{model:'x',provider:'a',providerName:'Shared'},{model:'x',provider:'b',providerName:'Shared'},{model:'y',provider:'b',providerName:'Shared'}];
 return [modelLabels(catalog.slice(0,2)).map(row=>row.label),modelLabels(catalog).map(row=>row.label),
  modelLabels([{model:'["route","r"]'}]).map(row=>row.label),
  modelLabels([{model:'["route","r"]'},{model:'plain',displayName:'Plain'}],'Named connection').map(row=>row.label)];
}),[['Shared · x','Shared · x'],['Shared (a) · x','Shared · x','Shared (b) · y'],['route · r'],['Named connection · r','Named connection · Plain']]);
await page.locator('[data-ai-edit="'+id+'"]').click();await page.evaluate(()=>{connectionEditor.models=[{model:'["source-a","shared"]',displayName:'Shared',provider:'source-a',providerName:'Station'},{model:'["source-b","shared"]',displayName:'Shared',provider:'source-b',providerName:'Station'}];connectionEditor.catalog=structuredClone(connectionEditor.models);renderConnectionModels();});assert.deepEqual(await page.locator('.model-catalog-text small').allTextContents(),['Station (source-a)','Station (source-b)']);assert.deepEqual(await page.locator('.model-catalog-text strong').allTextContents(),['Shared','Shared']);
const selectionDialogs=dialogs,modelReads=await page.evaluate(()=>requests.filter(r=>r.method==='models').length);
await page.locator('[data-ai-model-search]').fill('SOURCE-A');assert.equal(await page.locator('[data-ai-model-toggle]').count(),1);await page.locator('[data-ai-model-invert]').click();assert.deepEqual(await page.evaluate(()=>connectionEditor.models.map(m=>m.model)),['["source-b","shared"]']);await page.locator('[data-ai-model-all]').click();assert.equal(await page.evaluate(()=>connectionEditor.models.length),2);await page.locator('[data-ai-model-search]').fill('not-present');assert.equal(await page.locator('[data-ai-model-toggle]').count(),0);assert((await page.locator('#connection-models').innerText()).includes('没有匹配'));await page.locator('[data-ai-model-search]').fill('shared');assert.equal(await page.locator('[data-ai-model-toggle]:checked').count(),2);
await page.locator('[data-ai-model-toggle="0"]').uncheck();assert.equal(await page.locator('[data-ai-model-toggle]:checked').count(),1);await page.locator('[data-ai-model-invert]').click();assert.deepEqual(await page.evaluate(()=>connectionEditor.models.map(m=>m.model)),['["source-a","shared"]']);await page.locator('[data-ai-model-all]').click();assert.equal(await page.locator('[data-ai-model-toggle]:checked').count(),2);await page.locator('[data-ai-model-invert]').click();assert.equal(await page.locator('[data-ai-model-toggle]:checked').count(),0);const beforeEmptySave=await page.evaluate(()=>requests.filter(r=>r.method==='connection_save').length);await page.locator('[data-ai-save]').click();assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='connection_save').length),beforeEmptySave);assert((await page.locator('#connection-status').innerText()).includes('至少勾选一个模型'));await page.locator('[data-ai-model-all]').click();await page.locator('[data-ai-model-toggle="0"]').uncheck();assert.equal(dialogs,selectionDialogs);assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='models').length),modelReads);await page.locator('#settings-connection').screenshot({path:path.join(out,'model-selection.png')});await page.locator('[data-ai-save]').click();await page.locator('#connection-form').waitFor({state:'detached'});
assert.deepEqual(await page.evaluate(()=>fixture.config.ai_connections[0].models.map(m=>m.model)),['["source-b","shared"]']);
await page.locator('#cfg-model').selectOption('["source-b","shared"]');await page.locator('#page-title').click();await page.evaluate(()=>flushSettings());await page.waitForFunction(()=>fixture.config.model_profiles.analysis.model==='["source-b","shared"]');
// Manual API fallback, failures and secret isolation remain unchanged.
await page.locator('[data-ai-add]').click();await page.locator('[data-ai-driver]').selectOption('api');await page.locator('[data-ai-name]').fill('API fixture');await page.locator('[data-ai-base]').fill('https://example.com/v1');await page.locator('[data-ai-key]').fill('synthetic-secret');await page.locator('[data-ai-models]').click();await page.waitForFunction(()=>document.querySelector('#connection-status').textContent.includes('检测失败'));const saves=await page.evaluate(()=>requests.filter(r=>r.method==='connection_save').length);await page.locator('[data-ai-save]').click();assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='connection_save').length),saves);await page.locator('[data-ai-model-id]').fill('manual-model');await page.locator('[data-ai-model-add]').click();await page.locator('[data-ai-save]').click();await page.locator('#connection-form').waitFor({state:'detached'});assert(!(await page.evaluate(()=>JSON.stringify(fixture.config))).includes('synthetic-secret'));
assert((await page.locator('#ai-connections').innerText()).includes('OpenAI Chat Completions 兼容 API'));
const api=await page.evaluate(()=>fixture.config.ai_connections.find(c=>c.driver==='api').id);assert.equal(await page.locator('#cfg-handling-connection option[value="'+api+'"]').count(),0);
await page.locator('#cfg-analysis-connection').selectOption(api);await page.locator('#cfg-model').selectOption('manual-model');await page.locator('#cfg-effort option[value="xhigh"]').waitFor({state:'attached'});await page.locator('#cfg-effort').selectOption('high');await page.evaluate(()=>flushSettings());assert.equal(await page.evaluate(()=>fixture.config.model_profiles.analysis.reasoning_effort),'high');assert((await page.locator('#cfg-effort').getAttribute('title')).includes('未声明'));
await page.evaluate(()=>{const c=state.config.ai_connections.find(c=>c.driver==='api');c.models[0].reasoningEffortSupport='declared';c.models[0].supportedReasoningEfforts=[];loadModels();});assert.equal(await page.locator('#cfg-effort option').count(),1);await page.locator('#cfg-analysis-connection').selectOption(id);await page.evaluate(()=>flushSettings());await page.locator('[data-ai-remove="'+api+'"]').click();await page.waitForFunction(()=>fixture.config.ai_connections.length===1);
// Detection cannot overwrite manual path edits.
await page.locator('[data-ai-edit="'+id+'"]').click();await page.locator('[data-ai-path]').fill('');await page.evaluate(()=>{void detectConnection();document.querySelector('[data-ai-path]').value='manual.exe';});await page.waitForTimeout(60);assert.equal(await page.locator('[data-ai-path]').inputValue(),'manual.exe');await page.locator('[data-ai-cancel]').click();await page.locator('#connection-form').waitFor({state:'detached'});
// Selecting a detected replacement updates saved-connection drafts, never their stored config.
await page.evaluate(()=>{fixture.config.codex='broken.exe';acceptConnectionConfig(fixture.config);});
for(const connectionId of ['',id]){
 await page.locator('[data-ai-edit="'+connectionId+'"]').click();
 const original=await page.locator('[data-ai-path]').inputValue();
 const saved=await page.evaluate(()=>structuredClone(state.config));
 const saves=await page.evaluate(()=>requests.filter(r=>r.method==='connection_save').length);
 const selectedModels=await page.evaluate(()=>connectionEditor.models.map(m=>m.model));
 await page.locator('[data-ai-name]').fill('Retained connection draft');
 await page.evaluate(()=>{window.detectedCodex='broken.exe';window.detectedDsh=[{path:'broken.exe',label:'Broken'},{path:'second.exe',label:'Replacement'},{path:'slow.exe',label:'Slow'}];});
 await page.locator('[data-ai-detect]').click();
 await page.waitForFunction(()=>!connectionEditor.busy);
 assert.equal(await page.locator('[data-ai-path]').inputValue(),original,'detection alone retains the draft');
 await page.locator('[data-ai-candidates]').selectOption('broken.exe');
 await page.waitForFunction(()=>document.querySelector('#connection-status').textContent.includes('模拟检测失败'));
 assert.equal(await page.locator('[data-ai-path]').inputValue(),'broken.exe');
 assert.deepEqual(await page.evaluate(()=>connectionEditor.models.map(m=>m.model)),selectedModels,'failed refresh keeps selected models');
 assert.equal(await page.locator('[data-ai-name]').inputValue(),'Retained connection draft');
 assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='connection_save').length),saves);
 assert.deepEqual(await page.evaluate(()=>state.config),saved);
 await page.locator('[data-ai-cancel]').click();
 await page.locator('[data-ai-edit="'+connectionId+'"]').click();
 assert.equal(await page.locator('[data-ai-path]').inputValue(),original,'cancel leaves stored path intact');
 await page.evaluate(()=>window.detectedCodex='second.exe');
 await page.locator('[data-ai-detect]').click();await page.waitForFunction(()=>!connectionEditor.busy);
 await page.locator('[data-ai-candidates]').selectOption('second.exe');
 await page.waitForFunction(()=>document.querySelector('#connection-models').textContent.includes('second-model'));
 assert.equal(await page.locator('[data-ai-path]').inputValue(),'second.exe');
 assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='connection_save').length),saves,'existing connections require Save');
 assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='models').at(-1).p.connection.executable),'second.exe');
 if(!connectionId)await page.locator('#connection-form').screenshot({path:path.join(out,'detected-replacement.png')});
 await page.locator('[data-ai-save]').click();await page.locator('#connection-form').waitFor({state:'detached'});
 assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='connection_save').at(-1).p.expected.executable),original,'save retains the original concurrency snapshot');
 await page.locator('[data-ai-edit="'+connectionId+'"]').click();
 assert.equal(await page.locator('[data-ai-path]').inputValue(),'second.exe');
 await page.locator('[data-ai-detect]').click();await page.waitForFunction(()=>!connectionEditor.busy);
 assert.equal(await page.locator('#connection-form').count(),1,'same-path detection keeps an existing editor open');
 assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='connection_save').length),saves+1);
 await page.locator('[data-ai-cancel]').click();
}
// A delayed candidate refresh cannot overwrite a newer candidate or a reopened editor.
await page.locator('[data-ai-edit="'+id+'"]').click();
await page.locator('[data-ai-detect]').click();await page.waitForFunction(()=>!connectionEditor.busy);
await page.locator('[data-ai-candidates]').selectOption('slow.exe');
await page.waitForFunction(()=>requests.filter(r=>r.method==='models').at(-1).p.connection.executable==='slow.exe');
await page.locator('[data-ai-candidates]').selectOption('second.exe');
await page.waitForFunction(()=>document.querySelector('#connection-status').textContent.includes('已读取'));
await page.waitForTimeout(350);
assert.equal(await page.locator('[data-ai-path]').inputValue(),'second.exe');
assert(!(await page.locator('#connection-models').innerText()).includes('stale-model'));
await page.locator('[data-ai-candidates]').selectOption('slow.exe');
await page.waitForFunction(()=>requests.filter(r=>r.method==='models').at(-1).p.connection.executable==='slow.exe');
await page.locator('[data-ai-cancel]').click();await page.locator('[data-ai-edit="'+id+'"]').click();
await page.waitForTimeout(350);
assert.equal(await page.locator('[data-ai-path]').inputValue(),'second.exe');
assert(!(await page.locator('#connection-models').innerText()).includes('stale-model'));
await page.locator('[data-ai-cancel]').click();
// Committing a manually entered path keeps model rows in place, including on failure.
await page.locator('[data-ai-edit=""]').click();
const beforeManual=await page.evaluate(()=>connectionEditor.models.map(m=>m.model));
await page.locator('[data-ai-path]').fill('broken.exe');
await page.locator('[data-ai-models]').click();
await page.waitForFunction(()=>document.querySelector('#connection-status').textContent.includes('模拟检测失败'));
assert.deepEqual(await page.evaluate(()=>connectionEditor.models.map(m=>m.model)),beforeManual);
await page.locator('[data-ai-cancel]').click();await page.locator('#connection-form').waitFor({state:'detached'});
// Overlapping discovery must keep the newest operation busy and preserve the saved catalog.
await page.locator('[data-ai-edit=""]').click();
const savedConfig=await page.evaluate(()=>JSON.stringify(state.config));
await page.evaluate(()=>{
  window.catalogIpc=window.ipc.postMessage;
  window.catalogRequests=[];
  window.ipc.postMessage=payload=>{
    const request=JSON.parse(payload);
    if(request.method==='models')catalogRequests.push(request);
    else catalogIpc(payload);
  };
});
for(const failFirst of [false,true]){
  const previousModels=await page.evaluate(()=>connectionEditor.models);
  await page.evaluate(()=>{
    catalogRequests.length=0;
    window.firstCatalog=discoverConnectionModels();
    window.secondCatalog=discoverConnectionModels();
  });
  await page.waitForFunction(()=>catalogRequests.length===2);
  await page.evaluate(async fail=>{
    window.receive({id:catalogRequests[0].id,ok:!fail,error:'stale failure',data:{models:[{model:'stale'}]}});
    await firstCatalog;
  },failFirst);
  assert(await page.evaluate(()=>Boolean(connectionEditor.busy)));
  assert.deepEqual(await page.evaluate(()=>connectionEditor.models),previousModels);
  assert(!(await page.locator('#connection-status').innerText()).includes('stale'));
  await page.evaluate(async()=>{
    window.receive({id:catalogRequests[1].id,ok:true,data:{models:[{model:'current'}]}});
    await secondCatalog;
  });
  assert.equal(await page.evaluate(()=>Boolean(connectionEditor.busy)),false);
  assert.deepEqual(await page.evaluate(()=>connectionEditor.models.map(m=>m.model)),['current']);
}
await page.evaluate(()=>{
  catalogRequests.length=0;
  window.changedCatalog=discoverConnectionModels();
  document.querySelector('[data-ai-path]').value='changed-after-request.exe';
});
await page.evaluate(async()=>{
  window.receive({id:catalogRequests[0].id,ok:true,data:{models:[{model:'stale-after-edit'}]}});
  await changedCatalog;
  window.ipc.postMessage=catalogIpc;
});
assert.equal(await page.evaluate(()=>Boolean(connectionEditor.busy)),false);
assert.deepEqual(await page.evaluate(()=>connectionEditor.models.map(m=>m.model)),['current']);
assert.equal(await page.evaluate(()=>JSON.stringify(state.config)),savedConfig);
await page.locator('[data-ai-cancel]').click();
await page.locator('#settings-connection').screenshot({path:path.join(out,'connections.png')});await page.locator('#settings-models').screenshot({path:path.join(out,'model-routes.png')});await page.screenshot({path:path.join(out,'settings.png'),fullPage:true});
for(const width of [1000,760]){await page.setViewportSize({width,height:900});assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),'settings fit '+width);}
assert.deepEqual(errors,[]);console.log('PASS editor blank-space focus boundary; explicit ACP setup; model select-all/invert/subset save; collapsed connections; cancellation; DSH detection; model catalog; mandatory manual fallback; no secret persistence in config; atomic editor save/failure; stale detection/model results; independent routes; narrow layout. '+out);
}).catch(e=>{console.error(e);process.exitCode=1});
