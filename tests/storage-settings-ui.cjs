const fs=require('fs'),path=require('path'),assert=require('assert'),{chromium}=require('playwright');
const out=fs.mkdtempSync(path.join(require('os').tmpdir(),'shishi-storage-settings-'));
const model={model:'synthetic-model',displayName:'Synthetic model',supportedReasoningEfforts:[]};
const fixture={config:{codex:'synthetic-codex.exe',codex_name:'Synthetic Codex',codex_models:[model],model:'synthetic-model',leaders:[],ai_connections:[{id:'dsh',driver:'dsh',name:'Synthetic DSH',executable:'synthetic-dsh.cmd',models:[model]},{id:'api',driver:'api',name:'Synthetic API',base_url:'https://api.example.com/v1',models:[model]}],analysis_workdir:'D:/Synthetic/existing-ai-workspace',interval_minutes:5,analysis_interval_minutes:30,analysis_lookback_hours:24,daily_calls:40,max_input_chars:60000,message_retention_days:90,work_start:'09:00',work_end:'18:00'},root:'D:/Synthetic/app',data_directory:'D:/Synthetic/old-data',tasks:[],schedules:[],messages:[],usage:{},activity:[],audit:[],reports:[],suggestions:[],chat_sources:[],handling:[],tag_state:{groups:[],filters:{}}};
const mock=`window.fixture=${JSON.stringify(fixture)};window.requests=[];window.picked='D:/Synthetic/new-data';window.failStorage=false;window.ipc={postMessage(raw){const {id,method,params:p}=JSON.parse(raw);requests.push({method,p});setTimeout(()=>{let data=true;if(method==='state')data=fixture;if(method==='pick')data=picked;if(method==='models')data={models:[${JSON.stringify(model)}]};if(method==='settings_patch'){Object.assign(fixture.config,p);data={config:fixture.config};}if(method==='storage_change'&&failStorage){window.receive({id,ok:false,error:'Synthetic directory is occupied'});return;}window.receive({id,ok:true,data});},20)}};`;
const html=require('../scripts/frontend.cjs').build().replace('<script>','<script>'+mock+'\n');
(async()=>{const browser=await chromium.launch({headless:true,executablePath:['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe','C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync),args:['--disable-background-networking']});try{
 const errors=[];
 async function fresh(){const p=await browser.newPage({viewport:{width:1280,height:940}});p.on('pageerror',e=>errors.push(e.message));await p.setContent(html);await p.waitForFunction(()=>state?.config);await p.evaluate(()=>{page='settings';render();});return p;}
 const p=await fresh();let dialogs=0;p.on('dialog',async d=>{dialogs++;await d.dismiss();});
 assert.equal(await p.locator('#cfg-analysis-workdir,[data-pick="analysis-workdir"]').count(),0);
 assert.equal(await p.locator('.settings-path').innerText(),'D:/Synthetic/old-data');
 await p.locator('[data-storage-change]').click();await p.locator('[data-storage-apply="copy"]').waitFor({state:'visible'});assert(await p.locator('[data-storage-apply="copy"]').isVisible());assert(await p.locator('[data-storage-apply="switch"]').isVisible());
 await p.locator('#dialog [data-action="close"]').click();await p.waitForFunction(()=>document.querySelector('#overlay').hidden);
 assert.equal(await p.evaluate(()=>requests.filter(r=>r.method==='storage_change').length),0);
 await p.evaluate(()=>window.picked=null);await p.locator('[data-storage-change]').click();await p.waitForFunction(()=>!storageChoosing);assert(await p.locator('#overlay').isHidden());
 await p.evaluate(()=>window.picked=fixture.data_directory);await p.locator('[data-storage-change]').click();await p.waitForFunction(()=>!storageChoosing);assert(await p.locator('#overlay').isHidden());
 await p.evaluate(()=>{window.picked='D:/Synthetic/new-data';window.failStorage=true;});await p.locator('#cfg-budget').fill('42');
 await p.locator('[data-storage-change]').click();await p.locator('[data-storage-apply="copy"]').click();
 await p.waitForFunction(()=>document.querySelector('#storage-change-error').textContent.includes('occupied'));
 assert(!(await p.locator('[data-storage-apply="copy"]').isDisabled()));assert.equal(await p.evaluate(()=>state.data_directory),'D:/Synthetic/old-data');
 assert.equal(await p.evaluate(()=>fixture.config.analysis_workdir),'D:/Synthetic/existing-ai-workspace');
 assert.equal(await p.evaluate(()=>requests.find(r=>r.method==='settings_patch').p.analysis_workdir),undefined);
 assert.equal(await p.evaluate(()=>fixture.config.daily_calls),42);
 await p.evaluate(()=>window.failStorage=false);await p.locator('[data-storage-apply="copy"]').click();
 await p.waitForFunction(()=>requests.filter(r=>r.method==='storage_change').length===2);
 await p.locator('[data-storage-apply="copy"]').dispatchEvent('click');assert.equal(await p.evaluate(()=>requests.filter(r=>r.method==='storage_change').length),2);
 assert.deepEqual(await p.evaluate(()=>requests.filter(r=>r.method==='storage_change').map(r=>r.p)),[{directory:'D:/Synthetic/new-data',copy:true},{directory:'D:/Synthetic/new-data',copy:true}]);
 await p.evaluate(()=>closeModal());assert(await p.locator('#overlay').isVisible(),'cannot close after the migration helper has started');
 await p.locator('#dialog').screenshot({path:path.join(out,'directory-confirmation.png')});await p.close();
 const n=await fresh();await n.locator('[data-storage-change]').click();await n.locator('[data-storage-apply="switch"]').click();await n.waitForFunction(()=>requests.some(r=>r.method==='storage_change'));
 assert.deepEqual(await n.evaluate(()=>requests.find(r=>r.method==='storage_change').p),{directory:'D:/Synthetic/new-data',copy:false});await n.close();

 // Use real mouse gestures. A generated click on a common ancestor after a
 // drag is not a second outside click, even when it lands in the settings rail.
 for(const [id,selectors] of [['',['[data-ai-name]','[data-ai-path]']],['dsh',['[data-ai-name]','[data-ai-path]']],['api',['[data-ai-name]','[data-ai-base]','[data-ai-key]']]]){
  const page=await fresh();let prompts=0;page.on('dialog',async d=>{prompts++;await d.dismiss();});
  await page.locator(`[data-ai-edit="${id}"]`).click();
  for(const selector of selectors){
   const field=page.locator(selector);await field.fill(selector.includes('base')?'https://api.example.com/synthetic':selector.includes('path')?'synthetic-launcher.exe':'Synthetic selected text');
   await field.scrollIntoViewIfNeeded();const b=await field.boundingBox();
   await page.mouse.move(b.x+b.width-9,b.y+b.height/2);await page.mouse.down();
   await page.mouse.move(4,b.y+b.height/2,{steps:18});await page.waitForTimeout(130);await page.mouse.up();await page.waitForTimeout(30);
   assert.equal(await page.locator('#connection-form').count(),1,id+' '+selector+' drag preserves editor');assert.equal(prompts,0,'drag has no discard prompt');
   const selection=await field.evaluate(el=>({start:el.selectionStart,end:el.selectionEnd,length:el.value.length}));
   assert(selection.end>selection.start,'drag retains selected text: '+id+' '+selector);
  }
  await page.locator('#page-title').click();assert.equal(prompts,0,'a fresh outside click preserves edits without a prompt');assert.equal(await page.locator('#connection-form').count(),1,'outside click keeps draft');
  await page.locator('[data-ai-cancel]').click();assert.equal(prompts,0,'explicit cancel discards without a prompt');await page.close();
 }
 assert.equal(dialogs,0);assert.deepEqual(errors,[]);console.log('PASS directory copy/switch/cancel, async errors, saved settings, hidden AI workdir, duplicate protection, and real drag selection for Codex/DSH/API. '+out);
}finally{await browser.close();}})().catch(e=>{console.error(e);process.exitCode=1});
