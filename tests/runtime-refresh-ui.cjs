const fs = require('fs'), assert = require('assert'), {withBrowser} = require('./ui-driver.cjs');
const fixture = {tasks:[],handling:[],schedules:[],messages:[],activity:[],audit:[],reports:[],suggestions:[],chat_sources:[],usage:{},config:{enabled:false,auto_ai:false,leaders:[],ai_connections:[]},core_revision:'1'};
const mock = `window.fixture=${JSON.stringify(fixture)};window.requests=[];window.runtimeCalls=0;window.delayNext=0;window.ipc={postMessage(raw){const r=JSON.parse(raw);requests.push(r);let data=true;const delay=delayNext;delayNext=0;if(r.method==='state'){if(r.params.core_revision===fixture.core_revision){runtimeCalls++;data={_unchanged:true,_page:r.params.page,core_revision:fixture.core_revision,ai_busy:false,ai_progress:'Synthetic live progress',activity:[]};}else data={...structuredClone(fixture),_page:r.params.page};}setTimeout(()=>receive({id:r.id,ok:true,data}),delay);}};`;
withBrowser(async(browser)=>{
  const page=await browser.newPage();const errors=[];page.on('pageerror',e=>errors.push(e.message));
  await page.setContent(require('./ui-bundle.cjs')().replace('<script>','<script>'+mock));
  await page.waitForFunction(()=>window.requests.some(r=>r.method==='ui_ready'));
  await page.evaluate(async()=>{await refreshState(true);window.savedView=document.querySelector('#view').firstElementChild;window.savedRenderEpoch=renderEpoch;const input=document.createElement('input');input.id='synthetic-draft';input.value='Retained draft';document.querySelector('#view').append(input);input.focus();input.setSelectionRange(3,3);await refreshState(false);});
  assert(await page.evaluate(()=>runtimeCalls>0&&renderEpoch===savedRenderEpoch&&document.querySelector('#view').firstElementChild===savedView&&document.activeElement.id==='synthetic-draft'&&document.activeElement.selectionStart===3&&document.activeElement.value==='Retained draft'));
  await page.evaluate(async()=>{document.querySelector('#synthetic-draft').remove();fixture.tasks=[{id:'task',title:'New durable task',status:'待处理',today:true,created:Date.now(),updated:Date.now(),evidence:{}}];fixture.core_revision='2';await refreshState(false);});
  assert((await page.locator('#view').innerText()).includes('New durable task'));
  assert(await page.evaluate(()=>renderEpoch>savedRenderEpoch));
  // An in-flight response from the old page must not replace a newly selected page.
  await page.evaluate(async()=>{delayNext=80;const pending=refreshState(false);page='schedules';await render();await pending;});
  assert(await page.evaluate(()=>state._page==='schedules'&&requests.filter(r=>r.method==='state').at(-1).params.page==='schedules'));
  assert.deepEqual(errors,[]);
  console.log('PASS runtime refresh preserves DOM, drafts and caret; durable updates render; stale navigation responses are discarded.');
 }).catch(e=>{console.error(e);process.exitCode=1;});
