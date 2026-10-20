const fs=require('fs'),path=require('path'),assert=require('assert');
const {chromium}=require('playwright');
const root=path.resolve(__dirname,'..'),out=fs.mkdtempSync(path.join(require('os').tmpdir(),'shishi-ui-'));fs.mkdirSync(out,{recursive:true});
const time=Date.now();
const state={tasks:[],schedules:[],config:{codex:'',enabled:true,auto_ai:true,interval_minutes:30,morning_time:'08:30',evening_time:'17:30',archive_time:'21:00',work_start:'09:00',work_end:'18:00',leaders:[],daily_calls:40,model:'',reasoning_effort:''},messages:[{id:'m1',source_id:'one',source_name:'示例来源',sender:'测试用户',group:'测试会话',text:'发布测试通知',time,private:true}],chat_sources:[],activity:[],audit:[],suggestions:[],reports:[],usage:{day:'2026-10-15',calls:0,input_tokens:0,output_tokens:0,cached_input_tokens:0,cached_reports:0,unreported_calls:0},pending:1,last_scan:time,ai_busy:false,busy:false,root:'隔离测试数据',rule_version:0,in_work_hours:false};
const plugin=JSON.parse(fs.readFileSync(path.join(root,'examples/json-source/plugin.json'),'utf8'));
const history={versions:[{id:'v1',target:'classify',content:'只有明确完成证据才标记完成。',reason:'初始版本',action:'initial',created:time-1000}],current:{classify:'v1'},reviews:[{id:'r1',day:'2026-10-15',created:time,status:'ready',base_version:'v1',base_prompt:'只有明确完成证据才标记完成。',proposed_prompt:'只有明确完成证据才标记完成。收到仅代表确认接收。',reason:'人工修正说明“收到”不代表完成。',evidence_ids:['audit:1']}]};
const mock=`window.fixture=${JSON.stringify(state)};window.fixturePlugin=${JSON.stringify(plugin)};window.fixtureHistory=${JSON.stringify(history)};window.requests=[];window.ipc={postMessage(raw){const {id,method,params:p}=JSON.parse(raw);window.requests.push({method,p});setTimeout(()=>{if(['plugin_install','plugin_upload'].includes(method)&&window.fixtureInstallError){window.receive({id,ok:false,error:window.fixtureInstallError});return;}let data=true;const s=window.fixture,h=window.fixtureHistory;s.evolution={prompts:h.reviews};s.prompt_state={versions:h.versions.length,reviews:h.reviews.map(r=>[r.id,r.status,r.error])};switch(method){case 'settings_patch':{const profiles={...fixture.config.model_profiles,...p.model_profiles};Object.assign(fixture.config,p);if(p.model_profiles)fixture.config.model_profiles=profiles;data={config:fixture.config};break;}case 'state':data=s;break;case 'messages_query':{const q=(p.query||'').toLowerCase(),all=s.messages.filter(m=>!m.withdrawn&&m.time>=(p.since??0)&&m.time<=(p.until??Infinity)&&(!q||[m.text,m.sender,m.group,m.source_name].some(x=>(x||'').toLowerCase().includes(q)))).sort((a,b)=>b.time-a.time||b.id.localeCompare(a.id)),size=p.page_size||50,pages=Math.max(1,Math.ceil(all.length/size)),pg=Math.min(p.page||1,pages);data={rows:all.slice((pg-1)*size,pg*size),total:all.length,page:pg,pages,page_size:size};break;}case 'plugins':data={plugins:[window.fixturePlugin],sources:s.chat_sources,errors:[],legacy_available:false};break;case 'pick':data='D:/fixture/plugin.zip';break;case 'plugin_upload':case 'plugin_install':data=window.fixturePlugin;break;case 'source_discover':data=window.discoveryResult||{candidates:[{label:'有效聊天文件',config:{file:'D:/fixture/messages.json',scan_root:'D:/fixture'}}],warnings:[]};break;case 'source_save':data={...p.source,id:p.source.id||'source-one',updated:Date.now()};s.chat_sources=[data];break;case 'prompt_history':data=h;break;case 'prompt_adopt':h.current.classify='v2';h.versions.push({id:'v2',target:'classify',content:h.reviews[0].proposed_prompt,action:'adopt',reason:h.reviews[0].reason,created:Date.now()});h.reviews[0].status='adopted';break;case 'prompt_rollback':h.current.classify='v3';h.versions.push({...h.versions[0],id:'v3',action:'rollback',created:Date.now()});break;case 'prompts':data={classify:'default',format:'default',learning:'default',report:'default'};break;case 'models':data={models:[]};break;case 'evidence':data=[];break;}window.receive({id,ok:true,data});},window.rpcDelay?.[method]||5);}};`;
const html=require('../scripts/frontend.cjs').build().replace('<script>','<script>'+mock+'\n');
(async()=>{
 const executable=['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe','C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync);
 const browser=await chromium.launch({headless:true,executablePath:executable,args:['--disable-background-networking']});
 try{const page=await browser.newPage({viewport:{width:1280,height:900}});const errors=[];page.on('pageerror',e=>errors.push(e.message));await page.setContent(html);await page.waitForFunction(()=>!!window.fixture&&document.querySelector('#page-title')?.textContent==='事项');

 // The refresh glyph must be centered inside its button so rotation stays in place.
 const glyph=await page.evaluate(()=>{const b=document.querySelector('#top-actions [data-action="scan"]'),g=b?.querySelector('.refresh-glyph');if(!b||!g)return null;const br=b.getBoundingClientRect(),gr=g.getBoundingClientRect();return {dx:Math.abs((br.left+br.right-gr.left-gr.right)/2),dy:Math.abs((br.top+br.bottom-gr.top-gr.bottom)/2),w:br.width,h:br.height};});
 assert(glyph&&glyph.dx<=0.6&&glyph.dy<=0.6,'refresh glyph not centered: '+JSON.stringify(glyph));
 assert(glyph.w>=24&&glyph.h>=24,'refresh button too small: '+JSON.stringify(glyph));
 for(const scale of [1.25,1.5]){
  const zoomed=await browser.newPage({viewport:{width:1280,height:900},deviceScaleFactor:scale});
  await zoomed.setContent(html);await zoomed.waitForFunction(()=>!!window.fixture&&document.querySelector('#page-title')?.textContent==='事项');
  const g=await zoomed.evaluate(()=>{const b=document.querySelector('#top-actions [data-action="scan"]'),g=b?.querySelector('.refresh-glyph');if(!b||!g)return null;const br=b.getBoundingClientRect(),gr=g.getBoundingClientRect();return {dx:Math.abs((br.left+br.right-gr.left-gr.right)/2),dy:Math.abs((br.top+br.bottom-gr.top-gr.bottom)/2)};});
  assert(g&&g.dx<=0.6&&g.dy<=0.6,'refresh glyph not centered at '+scale+': '+JSON.stringify(g));
  await zoomed.close();
 }

 // Background polling must preserve DOM nodes on every idle page and coalesce overlap.
 for(const route of ['tasks','inbox','sources','prompts','schedules','reports','learning','logs']){
  await page.evaluate(async route=>{page=route;if(route==='prompts')promptSection='history';await render();},route);
  await page.evaluate(()=>{window.oldView=document.querySelector('#view').firstElementChild;window.oldStatus=document.querySelector('#status').firstElementChild;window.oldToolbar=document.querySelector('#top-actions').firstElementChild;window.oldScan=document.querySelector('#scan-controls').firstElementChild;});
  await page.evaluate(async()=>{await refreshState(false);await refreshState(false);});
  assert(await page.evaluate(()=>window.oldView===document.querySelector('#view').firstElementChild&&window.oldStatus===document.querySelector('#status').firstElementChild&&window.oldToolbar===document.querySelector('#top-actions').firstElementChild&&window.oldScan===document.querySelector('#scan-controls').firstElementChild),'idle DOM replaced on '+route);
 }
 await page.evaluate(async()=>{window.rpcDelay={state:75};const before=requests.filter(r=>r.method==='state').length;await Promise.all(Array.from({length:8},()=>refreshState(false)));window.coalesced=requests.filter(r=>r.method==='state').length-before;window.rpcDelay={};});assert.equal(await page.evaluate(()=>window.coalesced),1);
 // An explicit refresh during an old snapshot must wait for a fresh follow-up.
 await page.evaluate(async()=>{window.rpcDelay={state:60};const before=requests.filter(r=>r.method==='state').length;const first=refreshState(false);const second=refreshState(true);await second;window.forcedCount=requests.filter(r=>r.method==='state').length-before;await first;window.rpcDelay={};});assert.equal(await page.evaluate(()=>window.forcedCount),2);
 // Slow async page updates keep the current content, and stale responses cannot overwrite navigation.
 for(const route of ['sources','prompts']){
  await page.evaluate(async route=>{page=route;if(route==='prompts')promptSection='history';await render();},route);
  await page.evaluate(route=>{window.rpcDelay={[route==='sources'?'plugins':'prompt_history']:100};window.previousView=document.querySelector('#view').firstElementChild;window.slowRender=route==='sources'?renderSources():renderPromptHistory();},route);
  assert(await page.evaluate(()=>document.querySelector('#view').firstElementChild===window.previousView));
  await page.evaluate(async()=>{await window.slowRender;});assert(await page.evaluate(()=>document.querySelector('#view').firstElementChild===window.previousView));
  await page.evaluate(async route=>{window.slowRender=route==='sources'?renderSources():renderPromptHistory();page='inbox';render();await window.slowRender;window.rpcDelay={};},route);
  assert(await page.locator('#inbox-search').isVisible());
 }
 // Status changes invalidate the prompt page; unchanged polls do not refetch it.
 await page.evaluate(async()=>{page='learning';await render();window.fixtureHistory.reviews[0].status='pending';await refreshState(false);});
 assert(!(await page.locator('#view').innerText()).includes('等待生成'));
 assert.equal(await page.locator('.advice-page > .advice-card').count(),0);
 await page.evaluate(async()=>{window.fixtureHistory.reviews[0].status='ready';await refreshState(false);});
 assert.equal(await page.locator('.advice-page > .advice-card').count(),1);

 // One System Prompts destination contains advice and versions; archive links target advice.
 assert.equal(await page.locator('aside [data-page="learning"]').count(),0);assert.equal((await page.locator('aside [data-page="prompts"]').innerText()).replace(/\s/g,''),'✧系统提示词');
 await page.evaluate(async()=>{fixture.reports=[{label:'归档',date:'2026-10-16',time:Date.now(),tasks:[],delta:[],suggestions:[],pending_analysis:0}];page='reports';await refreshState(true);});await page.locator('.report-disclosure').filter({has:page.locator('[data-prompt-section="advice"]')}).locator('summary').click();await page.locator('[data-prompt-section="advice"]').click();assert.equal(await page.locator('#page-title').innerText(),'系统提示词');assert(await page.locator('.advice-page').isVisible());
 await page.evaluate(()=>{window.rpcDelay={prompt_history:150};});await page.locator('[role="tab"][data-prompt-section="history"]').click();await page.locator('[role="tab"][data-prompt-section="advice"]').click();await page.evaluate(()=>new Promise(r=>setTimeout(r,200)));assert(await page.locator('.advice-page').isVisible());await page.evaluate(()=>window.rpcDelay={});
 // Real DOM file drops exercise drag acceptance, byte transport, duplicate guards and cleanup.
 const zipBytes=[...fs.readFileSync(path.join(root,'tests/fixtures/plugins/root.zip'))];
 const drop=async(names,type='drop')=>page.evaluate(({names,type,bytes})=>{const dt=new DataTransfer();for(const name of names)dt.items.add(new File([new Uint8Array(bytes)],name,{type:'application/zip'}));Object.defineProperty(dt,'dropEffect',{value:'none',writable:true});const e=new DragEvent(type,{bubbles:true,cancelable:true,dataTransfer:dt});document.querySelector('#view').dispatchEvent(e);return {prevented:e.defaultPrevented,effect:dt.dropEffect};},{names,type,bytes:zipBytes});
 await page.evaluate(async()=>{page='inbox';await render();});await drop(['plugin.zip']);assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='plugin_upload').length),0);
 await page.evaluate(async()=>{page='sources';await render();});assert.deepEqual(await drop(['插件 包.ZIP'],'dragover'),{prevented:true,effect:'copy'});assert(await page.locator('#view.plugin-drag-over').count());await page.screenshot({path:path.join(out,'plugin-drop.png')});
 await drop(['bad.exe']);await drop(['one.zip','two.zip']);assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='plugin_upload').length),0);
 await page.evaluate(()=>{window.rpcDelay={plugin_upload:120};});await drop(['插件 包.ZIP']);assert(await page.locator('[data-extension="install-plugin"]').isDisabled());await drop(['again.zip']);await page.locator('#source-name').waitFor();await page.waitForFunction(()=>!pluginInstalling);
 assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='plugin_upload').length),1);assert.equal(await page.evaluate(()=>requests.find(r=>r.method==='plugin_upload').p.base64),Buffer.from(zipBytes).toString('base64'));
 await page.waitForFunction(()=>document.querySelector('#source-field-file').value==='D:/fixture/messages.json');assert.equal(await page.locator('#source-field-scan_root').inputValue(),'D:/fixture');assert.equal(await page.locator('#source-candidate').inputValue(),'0');
 await drop(['plugin.zip']);assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='plugin_upload').length),1);await page.locator('[data-action="close"]').click();
 await page.evaluate(()=>{window.fixtureInstallError='无效插件包';window.rpcDelay={};});await drop(['broken.zip']);await page.waitForFunction(()=>!pluginInstalling);assert((await page.locator('#toast').innerText()).includes('无效插件包'));assert(!(await page.locator('[data-extension="install-plugin"]').isDisabled()));await page.evaluate(()=>delete window.fixtureInstallError);
 // Discovery applies a whole candidate, preserves edits made while scanning and ignores closed forms.
 await page.evaluate(()=>{window.discoveryResult={candidates:[{label:'A',config:{file:'A.json',scan_root:'A'}},{label:'B',config:{file:'B.json'}}],warnings:[]};});await page.locator('[data-source-add]').click();await page.locator('#source-candidate').waitFor();assert.equal(await page.locator('#source-candidate').inputValue(),'');await page.locator('#source-candidate').selectOption('0');await page.locator('#source-candidate').selectOption('1');assert.equal(await page.locator('#source-field-file').inputValue(),'B.json');assert.equal(await page.locator('#source-field-scan_root').inputValue(),'');await page.locator('[data-action="close"]').click();
 await page.evaluate(()=>{delete window.discoveryResult;window.rpcDelay={source_discover:250};});await page.locator('[data-source-add]').click();await page.locator('#source-field-file').fill('manual.json');await page.waitForFunction(()=>!document.querySelector('[data-extension="source-discover"]').disabled);assert.equal(await page.locator('#source-field-file').inputValue(),'manual.json');assert.equal(await page.locator('#source-candidate').inputValue(),'');await page.locator('[data-action="close"]').click();
 await page.locator('[data-source-add]').click();await page.locator('[data-action="close"]').click();await page.evaluate(()=>new Promise(resolve=>setTimeout(resolve,300)));assert(await page.locator('#overlay').isHidden());await page.evaluate(()=>window.rpcDelay={});
 // ZIP picker and successful installation lead directly to plugin-defined configuration.
 await page.evaluate(async()=>{page='sources';await render();});
 await page.locator('[data-extension="install-plugin"]').click();await page.locator('#source-name').waitFor();
 assert(await page.evaluate(()=>requests.some(r=>r.method==='pick'&&r.p.type==='plugin')&&requests.some(r=>r.method==='plugin_install'&&r.p.path.endsWith('.zip'))));
 await page.locator('[data-action="close"]').click();
 // Updating or reinstalling an existing package must not open a duplicate source form or run discovery.
 for(const status of ['updated','unchanged']){
  const discoveries=await page.evaluate(()=>requests.filter(r=>r.method==='source_discover').length);
  await page.evaluate(status=>{fixturePlugin.install_status=status;},status);
  await drop(['upgrade.zip']);await page.waitForFunction(()=>!pluginInstalling);
  assert(await page.locator('#overlay').isHidden());
  assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='source_discover').length),discoveries);
  assert((await page.locator('#toast').innerText()).includes(status==='updated'?'来源配置和读取进度已保留':'无需重复安装'));
 }
 await page.evaluate(()=>delete fixturePlugin.install_status);
 await page.locator('[data-page="inbox"]').click();const input=page.locator('#inbox-search');await input.fill('发布');
 await page.evaluate(()=>{window.originalInput=document.querySelector('#inbox-search');originalInput.setSelectionRange(1,1);});
 for(let i=0;i<3;i++)await page.evaluate(()=>refreshState(false));
 assert(await page.evaluate(()=>document.activeElement===window.originalInput&&document.querySelector('#inbox-search')===window.originalInput&&originalInput.selectionStart===1));
 await page.evaluate(()=>{const input=window.originalInput;input.dispatchEvent(new CompositionEvent('compositionstart',{bubbles:true,data:'测'}));input.value='测试';input.dispatchEvent(new InputEvent('input',{bubbles:true,isComposing:true,data:'试'}));window.fixture.messages.push({id:'m2',sender:'另一用户',source_name:'另一来源',group:'会话',text:'测试新增消息',time:Date.now(),private:true});});
 await page.evaluate(()=>refreshState(false));assert(await page.evaluate(()=>document.activeElement===window.originalInput&&window.originalInput.value==='测试'));
 await page.evaluate(()=>originalInput.dispatchEvent(new CompositionEvent('compositionend',{bubbles:true,data:'测试'})));
 await page.waitForFunction(()=>document.querySelector('#inbox-content').textContent.includes('测试新增消息'));assert((await page.locator('#inbox-content').innerText()).includes('测试新增消息'));assert((await page.locator('#inbox-summary').innerText()).includes('自动保留 90 天'));
 await page.screenshot({path:path.join(out,'inbox.png')});
 await page.locator('[data-page="sources"]').click();await page.locator('[data-source-add]').click();assert(await page.locator('[data-source-pick="scan_root"]').isVisible());
 await page.waitForFunction(()=>document.querySelector('#source-candidate')?.value==='0');assert.equal(await page.locator('#source-field-file').inputValue(),'D:/fixture/messages.json');
 await page.screenshot({path:path.join(out,'source-config.png')});await page.locator('[data-extension="source-save"]').click();await page.waitForFunction(()=>document.querySelector('#overlay').hidden);await page.locator('[data-source-edit]').waitFor();
 await page.locator('aside [data-page="prompts"]').click();await page.locator('[role="tab"][data-prompt-section="advice"]').click();await page.locator('.advice-details > summary').click();assert((await page.locator('.advice-details').innerText()).includes('收到仅代表确认接收'));
 await page.screenshot({path:path.join(out,'prompt-review.png')});await page.locator('[data-advice-action="enable"]').click();await page.waitForFunction(()=>window.fixtureHistory.current.classify==='v2');await page.locator('[role="tab"][data-prompt-section="history"]').click();await page.locator(".prompt-history > summary").click();await page.locator(".prompt-version").filter({has:page.locator('[data-prompt-rollback="v1"]')}).locator("summary").click();await page.locator('[data-prompt-rollback="v1"]').click();await page.waitForFunction(()=>window.fixtureHistory.current.classify==='v3');
 await page.locator('[data-page="settings"]').first().click();await page.locator('#settings-connection').waitFor();assert.equal(await page.locator('#cfg-prompt-classify').count(),0);assert.equal(await page.locator('[data-ai-path]').count(),0);assert.equal(await page.locator('#cfg-database').count(),0);assert.equal(await page.locator('#cfg-auto-adopt-prompt,#cfg-auto-adopt').count(),0);assert(!(await page.locator('#settings-automation').innerText()).includes('6,000 字符补查预留'));
 assert.deepEqual(errors,[]);console.log('PASS idle DOM stability; refresh coalescing; forced follow-up; async no-blank/stale navigation; ZIP picker and drag/drop install; drop guards and recovery; inbox focus, caret, IME composition and new messages; plugin forms/discovery/save; prompt preview/adopt/rollback; settings; screenshots='+out);
 }finally{await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1});
