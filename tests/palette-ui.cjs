const assert=require('assert'),{chromium}=require('playwright'),fs=require('fs');
const bundle=require('./ui-bundle.cjs');
const fixture={config:{codex:'',enabled:true,auto_ai:true,interval_minutes:30,work_start:'09:00',work_end:'18:00',leaders:[],daily_calls:40},tasks:[{id:'t1',title:'Synthetic task',description:'Example',status:'待处理',priority:'普通',category:'日常事务',created:1,updated:2,today:true,tags:[]}],schedules:[],chat_sources:[],activity:[],suggestions:[],handling:[],audit:[],reports:[],evolution:{prompts:[],handling:[]},usage:{day:'2026-10-19',calls:0,input_tokens:0,output_tokens:0,cached_input_tokens:0,cached_reports:0,unreported_calls:0},messages_revision:[0,0,0],pending:0,last_scan:0,ai_busy:false,busy:false,root:'isolated',rule_version:0,tag_state:{groups:[],filters:{},proposals:[],versions:[]}};
const mock=`window.requests=[];window.ipc={postMessage(raw){const {id,method,params:p}=JSON.parse(raw);requests.push({method,p});setTimeout(()=>{let data=true;if(method==='state')data=JSON.parse(JSON.stringify(${JSON.stringify(fixture)}));if(method==='messages_query')data={rows:[],page:1,pages:1,total:0};if(method==='prompt_history')data={versions:[],current:{}};if(method==='plugins')data={plugins:[],sources:[],errors:[]};window.receive({id,ok:true,data});},5)}};`;
const html=bundle().replace('<script>','<script>'+mock+'\n');
(async()=>{const browser=await chromium.launch({headless:true,executablePath:['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe','C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync)});try{
 const page=await browser.newPage({viewport:{width:1280,height:900}}),errors=[];page.on('pageerror',e=>errors.push(e.message));
 await page.setContent(html);await page.waitForFunction(()=>document.querySelector('.task-row'));
 const title=()=>page.locator('#page-title').innerText();
 // Palette: open, filter, keyboard selection, run, close.
 await page.keyboard.press('Control+k');await page.locator('#palette-input').waitFor();
 assert(await page.locator('#palette li kbd').first().isVisible(),'shortcut hints are shown');
 await page.keyboard.type('收件箱');assert.equal(await page.locator('#palette li[role="option"]').count(),1);
 await page.keyboard.press('Enter');await page.waitForFunction(()=>document.querySelector('#page-title').textContent==='消息收件箱');
 assert.equal(await page.locator('#palette').count(),0,'palette closes after running');
 await page.keyboard.press('Control+k');await page.keyboard.press('ArrowDown');
 assert.equal(await page.locator('#palette li[aria-selected="true"]').innerText(),'前往 定时事项\nCtrl 2');
 await page.keyboard.press('Escape');assert.equal(await page.locator('#palette').count(),0);
 await page.locator('#palette-open').click();await page.locator('#palette-input').waitFor();await page.keyboard.press('Control+k');
 assert.equal(await page.locator('#palette').count(),0,'Ctrl+K toggles');
 // Global shortcuts.
 await page.keyboard.press('Control+1');assert.equal(await title(),'事项');
 await page.keyboard.press('Control+7');await page.waitForFunction(()=>document.querySelector('#page-title').textContent==='系统日志');
 await page.keyboard.press('Control+1');await page.keyboard.press('/');
 assert.equal(await page.evaluate(()=>document.activeElement.id),'task-search');
 await page.keyboard.type('/');assert.equal(await page.locator('#task-search').inputValue(),'/','"/" types normally inside inputs');
 await page.locator('#task-search').fill('');await page.locator('.task-row').first().click();
 const before=await page.evaluate(()=>requests.filter(r=>r.method==='state').length);
 await page.keyboard.press('F5');await page.waitForFunction(n=>requests.filter(r=>r.method==='state').length>n,before);
 assert.equal(await title(),'事项','F5 refreshes data instead of reloading the document');
 await page.keyboard.press('Control+n');await page.locator('#overlay:not([hidden])').waitFor();
 await page.keyboard.press('Control+k');assert.equal(await page.locator('#palette').count(),0,'no palette over a dialog');
 await page.keyboard.press('Control+3');assert.equal(await title(),'事项','page shortcuts are inert while a dialog is open');
 assert.equal(await page.locator('aside [data-page="inbox"]').getAttribute('aria-keyshortcuts'),'Control+3');
 // Mica: opaque unless the native side confirmed the backdrop; then only the base is transparent.
 const bg=()=>page.evaluate(()=>[getComputedStyle(document.body).backgroundColor,getComputedStyle(document.querySelector('.main')).backgroundColor,getComputedStyle(document.querySelector('.sidebar')).backgroundColor]);
 assert.deepEqual(await bg(),['rgb(32, 32, 32)','rgb(39, 39, 39)','rgb(32, 32, 32)']);
 await page.evaluate(()=>document.documentElement.dataset.backdrop='mica');
 assert.deepEqual(await bg(),['rgba(0, 0, 0, 0)','rgba(39, 39, 39, 0.96)','rgba(0, 0, 0, 0)']);
 assert.equal(await page.evaluate(()=>getComputedStyle(document.documentElement).backgroundColor),'rgba(32, 32, 32, 0.9)');
 assert.deepEqual(errors,[]);
 console.log('PASS Mica opaque fallback and transparent base; palette open/filter/select/run/toggle; Ctrl+1-8, Ctrl+N, "/" search, F5 data refresh; inert under dialogs; shortcut hints');
}finally{await browser.close();}})().catch(e=>{console.error(e);process.exitCode=1});
