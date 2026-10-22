// Offline clock-controlled contracts for daily cached-package notifications.
const fs=require('fs'),path=require('path'),os=require('os'),assert=require('assert'),{chromium}=require('playwright');
const out=fs.mkdtempSync(path.join(os.tmpdir(),'shishi-update-schedule-')),now=Date.now();
const fixture={version:'1.1.18',build_time:1700000000000,updates:{status:'ready',version:'9.0.0',progress:100},config:{auto_update:true,codex:'',codex_models:[],ai_connections:[],leaders:[],enabled:false,auto_ai:false,interval_minutes:5,analysis_interval_minutes:30,analysis_lookback_hours:24,daily_calls:40,max_input_chars:60000,message_retention_days:90,work_start:'09:00',work_end:'18:00'},tasks:[],schedules:[],messages:[],audit:[],activity:[],reports:[],handling:[],chat_sources:[],suggestions:[],usage:{calls:0,input_tokens:0,output_tokens:0,cached_reports:0,unreported_calls:0},pending:0};
const mock=`window.fixture=${JSON.stringify(fixture)};window.scheduleNow=${now};Date.now=()=>scheduleNow;window.requests=[];window.failInstall=false;
window.ipc={postMessage(raw){const {id,method,params:p}=JSON.parse(raw);requests.push({method,p});setTimeout(()=>{let data=true,error='';
if(method==='state')data=fixture;
if(method==='settings_patch'){Object.assign(fixture.config,p);data={config:fixture.config};}
if(method==='update_install'){if(failInstall)error='合成：本地工作尚未结束';else fixture.updates.status='installing';}
window.receive({id,ok:!error,data,error});},method==='update_install'?80:5);}};`;
(async()=>{const browser=await chromium.launch({headless:true,executablePath:['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe','C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync),args:['--disable-background-networking']});try{
 const page=await browser.newPage({viewport:{width:1100,height:800}}),errors=[];page.on('pageerror',e=>errors.push(e.message));
 await page.setContent(require('./ui-bundle.cjs')().replace('<script>','<script>'+mock));
 await page.locator('[data-page="settings"]').first().click();await page.locator('#settings-about').waitFor();
 assert(await page.locator('#overlay').isHidden(),'cached package at startup does not catch up');
 await page.evaluate(async()=>{fixture.updates.status='downloading';await refreshState(true);fixture.updates.status='ready';await refreshState(true);syncUpdateNotice();});
 assert(await page.locator('#overlay').isHidden(),'download completion alone does not notify');
 await page.evaluate(async()=>{fixture.updates.reminder={id:'morning-1',expires_at:scheduleNow+60000};await refreshState(true);syncUpdateNotice();});
 await page.locator('[data-update-confirm]').waitFor();assert.equal(await page.locator('[data-update-confirm]').innerText(),'立即更新');assert.equal(await page.locator('[data-update-later]').innerText(),'稍后更新');
 assert((await page.locator('#dialog').innerText()).includes('每天凌晨 04:00'));
 await page.screenshot({path:path.join(out,'morning-reminder.png')});
 await page.locator('[data-update-later]').click();await page.locator('#overlay').waitFor({state:'hidden'});
 await page.evaluate(async()=>{for(let n=0;n<3;n++){await refreshState(true);syncUpdateNotice();}});
 assert(await page.locator('#overlay').isHidden(),'daily reminder stays dismissed');
 assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='update_install').length),0,'later does not install now');
 await page.evaluate(async()=>{scheduleNow+=86400000;fixture.updates.reminder={id:'morning-2',expires_at:scheduleNow+60000};await refreshState(true);});
 await page.locator('[data-update-later]').click();await page.locator('#overlay').waitFor({state:'hidden'});
 await page.evaluate(async()=>{fixture.updates.reminder={id:'expired',expires_at:scheduleNow};await refreshState(true);});assert(await page.locator('#overlay').isHidden());
 await page.evaluate(async()=>{fixture.config.auto_update=false;fixture.updates.reminder={id:'disabled',expires_at:scheduleNow+60000};fixture.updates.automatic_install={id:'disabled-install',expires_at:scheduleNow+60000};await refreshState(true);});
 assert(await page.locator('#overlay').isHidden());assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='update_install').length),0);
 await page.locator('[data-update-check]').click();await page.locator('[data-update-confirm]').waitFor();assert(!(await page.locator('#dialog').innerText()).includes('每天凌晨'));
 await page.locator('[data-update-later]').click();await page.locator('#overlay').waitFor({state:'hidden'});
 await page.evaluate(async()=>{fixture.config.auto_update=true;fixture.updates.reminder=null;fixture.updates.automatic_install={id:'missed-install',expires_at:scheduleNow};await refreshState(true);});
 assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='update_install').length),0,'expired install is never replayed');
 await page.evaluate(async()=>{fixture.updates.automatic_install={id:'future-install',expires_at:scheduleNow+120000};await refreshState(true);});
 assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='update_install').length),0,'clock rewind is not due');
 await page.evaluate(async()=>{failInstall=true;fixture.updates.automatic_install={id:'four-1',expires_at:scheduleNow+60000};await refreshState(true);syncUpdateNotice();syncUpdateNotice();});
 await page.waitForFunction(()=>requests.some(r=>r.method==='update_install'&&r.p.scheduled==='four-1'));
 await page.locator('#toast').filter({hasText:'本地工作尚未结束'}).waitFor();
 await page.evaluate(async()=>{for(let n=0;n<3;n++){await refreshState(true);syncUpdateNotice();}});
 assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='update_install').length),1,'failed automatic install does not loop');
 await page.evaluate(async()=>{failInstall=false;scheduleNow+=86400000;fixture.updates.automatic_install={id:'four-2',expires_at:scheduleNow+60000};await refreshState(true);});
 await page.waitForFunction(()=>fixture.updates.status==='installing');
 assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='update_install').length),2);
 assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='update_install').at(-1).p.scheduled),'four-2');
 assert(await page.locator('#overlay').isHidden(),'04:00 update needs no reminder click');assert.deepEqual(errors,[]);
 console.log('PASS cached-package daily reminder; later/manual choices; next day; no startup/download/missed-time catch-up; disabled/expired/clock-rewind guards; scheduled installation and failure dedup. '+out);
}finally{await browser.close();}})().catch(e=>{console.error(e);process.exit(1)});
