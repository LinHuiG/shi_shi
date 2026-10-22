// Offline UI contracts for about/update state and scoped searchable task history.
const fs=require('fs'),path=require('path'),assert=require('assert'),{chromium}=require('playwright');
const out=fs.mkdtempSync(path.join(require('os').tmpdir(),'shishi-about-history-')),now=Date.now();
const fixture={version:'1.1.17',build_time:1700000000000,updates:{status:'idle',updated_at:1600000000000,installed_at:1500000000000},config:{auto_update:true,codex:'',codex_models:[],ai_connections:[],leaders:[],enabled:false,auto_ai:false,interval_minutes:5,analysis_interval_minutes:30,analysis_lookback_hours:24,daily_calls:40,max_input_chars:60000,message_retention_days:90,work_start:'09:00',work_end:'18:00'},tasks:[{id:'first',number:42,title:'合成核对事项',description:'合成事项描述',status:'待处理',today:true,created:now,updated:now},{id:'second',number:7,title:'另一个事项',description:'其他内容',status:'待处理',today:true,created:now,updated:now}],schedules:[],messages:[],audit:[],activity:[],reports:[],handling:[],chat_sources:[],suggestions:[],usage:{calls:0,input_tokens:0,output_tokens:0,cached_reports:0,unreported_calls:0},pending:0};
const mock=`window.fixture=${JSON.stringify(fixture)};window.requests=[];window.failUpdate=false;window.evidenceRows=Array.from({length:80},(_,i)=>({id:'m'+i,text:'合成消息依据 '+i+'：需要核对的说明。'.repeat(12),sender:'合成发送人',time:${now}+i}));
window.changes=Array.from({length:75},(_,i)=>({index:i,time:${now}-1000+i,kind:'manual_edit',task_id:'first',detail:i===74?'需要核对 Unicode 合成记录':'合成修改 '+i,before:{title:'原始标题'},after:{title:'新标题'}}));
window.ipc={postMessage(raw){const {id,method,params:p}=JSON.parse(raw);requests.push({method,p});setTimeout(()=>{let data=true,error='';
if(method==='state')data=fixture;
if(method==='evidence')data=evidenceRows;
if(method==='task_changes'){const rows=p.id==='first'?changes.filter(r=>p.query.trim().toLowerCase().split(/\\s+/).every(q=>JSON.stringify(r).toLowerCase().includes(q))).sort((a,b)=>p.sort==='asc'?a.time-b.time:b.time-a.time):[];data={rows:rows.slice((p.page-1)*50,p.page*50),all:p.id==='first'?75:0,total:rows.length,page:p.page,pages:Math.max(1,Math.ceil(rows.length/50))};}
if(method==='settings_patch'){Object.assign(fixture.config,p);data={config:fixture.config};}
if(method==='update_check'){if(failUpdate)error='合成网络失败';else{fixture.updates={status:'checking'};data=fixture.updates;setTimeout(()=>{fixture.updates.status='ready';fixture.updates.version='9.0.0';fixture.updates.progress=100;window.stateChanged('update-ready-'+Date.now());},250);}}
if(method==='update_install'){fixture.updates.status='installing';}
if(method==='activity_query')data={rows:[],day:p.day,page:1,pages:1,total:0};
if(method==='memory_read')data={temporary:{revision:1,max_chars:2000,text:'合成临时笔记'},archived:{revision:1,max_chars:4000,entries:[{id:'m',key:'核对',text:'合成长期经验'}]}};
window.receive({id,ok:!error,data,error});},method==='update_check'?80:5);}};`;
(async()=>{const browser=await chromium.launch({headless:true,executablePath:['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe','C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync),args:['--disable-background-networking']});try{
 const page=await browser.newPage({viewport:{width:1280,height:900}}),errors=[];page.on('pageerror',e=>errors.push(e.message));
 await page.setContent(require('./ui-bundle.cjs')().replace('<script>','<script>'+mock));
 await page.locator('#task-detail h2').waitFor();assert((await page.locator('#task-detail h2').innerText()).startsWith('#42'));
 assert.equal(await page.locator('.task-description.detail-panel').count(),1);assert.equal(await page.locator('.evidence-disclosure.detail-panel').count(),1);

 const evidence=page.locator('.evidence-disclosure'),pane=page.locator('.detail');await page.locator('[data-evidence-count]').filter({hasText:'80 条'}).waitFor();
 assert(await page.evaluate(()=>getComputedStyle(document.querySelector('.detail-panel')).backgroundColor===getComputedStyle(document.querySelector('.advice')).backgroundColor));
 async function checkBottomControl(panel){
  await panel.evaluate(el=>{const pane=el.closest('.detail');pane.scrollTop+=el.getBoundingClientRect().top-pane.getBoundingClientRect().top+180;});
  await page.waitForTimeout(180);
  const button=await panel.locator('[data-detail-collapse]').boundingBox(),bounds=await pane.boundingBox();
  assert(button.y>=bounds.y&&button.y+button.height<=bounds.y+bounds.height+1,'collapse is inside the actual visible scrollport');
  assert(bounds.y+bounds.height-button.y-button.height<12,'collapse floats at the visible lower edge');
  assert(await panel.evaluate(el=>!['#evidence','#task-changes'].some(selector=>{const body=el.querySelector(selector);return body&&body.clientHeight<body.scrollHeight-1;})),'record bodies expand fully');
  await panel.locator('[data-detail-collapse]').click();await page.waitForTimeout(180);assert.equal(await panel.getAttribute('open'),null);
  const summary=await panel.locator(':scope > summary').boundingBox();assert(summary.y>=bounds.y&&summary.y+summary.height<=bounds.y+bounds.height+1,'collapsed header remains reachable');
 }
 for(const size of [{width:1280,height:900},{width:760,height:700},{width:1280,height:540}]){
  await page.setViewportSize(size);await evidence.locator('summary').click();await checkBottomControl(evidence);
 }
 await page.setViewportSize({width:1280,height:900});await evidence.locator('summary').click();
 await page.locator('#evidence-search').fill('未匹配的合成关键词');assert.equal(await evidence.getAttribute('open'),'');assert.equal(await evidence.locator('summary').getAttribute('aria-disabled'),'false');
 await page.evaluate(async()=>{window.retainedEvidence=evidenceRows;evidenceRows=[];fixture.tasks[0].evidence_count=0;await refreshState(true);});
 await page.waitForFunction(()=>document.querySelector('.evidence-disclosure>summary').getAttribute('aria-disabled')==='true');
 assert.equal(await evidence.getAttribute('open'),null);await evidence.locator('summary').click();assert.equal(await evidence.getAttribute('open'),null);
 await evidence.locator('summary').evaluate(el=>el.focus());await page.keyboard.press('Enter');await page.keyboard.press('Space');assert.equal(await evidence.getAttribute('open'),null);
 assert(await page.evaluate(()=>evidenceView('first').open===false));
 await page.evaluate(async()=>{evidenceRows=retainedEvidence;fixture.tasks[0].evidence_count=evidenceRows.length;await refreshState(true);});
 await page.waitForFunction(()=>document.querySelector('.evidence-disclosure>summary').getAttribute('aria-disabled')==='false');assert.equal(await evidence.getAttribute('open'),null);
 const history=page.locator('.task-changes-disclosure');await history.locator(':scope > summary').click();await page.getByText('共 75 条',{exact:true}).waitFor();
 await checkBottomControl(history);await history.locator(':scope > summary').click();await page.getByText('共 75 条',{exact:true}).waitFor();
 assert.equal(await page.locator('.task-change-row').count(),50);assert((await page.locator('.task-change-row').first().innerText()).includes('Unicode'));
 await page.locator('[data-task-changes-sort]').click();await page.waitForFunction(()=>document.querySelector('.task-change-row')?.textContent.includes('合成修改 0'));
 await page.locator('#task-changes-search').fill('unicode');await page.getByText('匹配 1 / 共 75 条').waitFor();assert.equal(await page.locator('.task-change-row').count(),1);
 await page.locator('#task-changes-search').evaluate(el=>{el.value='中文';el.dispatchEvent(new CompositionEvent('compositionend',{bubbles:true,data:'中文'}));});await page.getByText('匹配 0 / 共 75 条').waitFor();
 await page.locator('#task-changes-search').fill('unicode');await page.getByText('匹配 1 / 共 75 条').waitFor();await page.evaluate(()=>refreshState(true));assert.equal(await page.locator('#task-changes-search').inputValue(),'unicode');assert.equal(await history.getAttribute('open'),'');
 await page.evaluate(()=>Promise.allSettled(document.getAnimations().map(a=>a.finished)));await page.screenshot({path:path.join(out,'task-history.png')});
 await page.locator('[data-page="settings"]').first().click();await page.locator('#settings-about').waitFor();assert(await page.locator('#cfg-auto-update').isChecked());assert((await page.locator('#settings-about').innerText()).includes('版本 1.1.17'));assert(!(await page.locator('.about-description').innerText()).includes('LinHuiG'));

 const buildLabel=await page.locator('#version-build-time').innerText();assert(buildLabel.startsWith('发布于 '));assert.equal(await page.locator('#update-history').count(),0);assert.equal(await page.locator('.about-head>.about-version + #version-build-time').count(),1);
 const aboutText=await page.locator('#settings-about').innerText();assert(!aboutText.includes('每半小时'));assert(!aboutText.includes('尚无自动更新记录'));
 assert.equal(await page.locator('.about-head #cfg-auto-update').count(),1);assert.equal(await page.locator('.about-head [data-update-check]').count(),1);assert.equal(await page.locator('.about-links a svg').count(),1);
 const head=await page.locator('.about-head h2').boundingBox(),check=await page.locator('[data-update-check]').boundingBox();assert(Math.abs(head.y-check.y)<10,'update controls share the title row');
 await page.locator('#settings-storage').scrollIntoViewIfNeeded();
 const directory=await page.locator('.data-directory .settings-path').boundingBox(),openDir=await page.locator('[data-action="open-data"]').boundingBox(),changeDir=await page.locator('[data-storage-change]').boundingBox(),retention=await page.locator('#cfg-retention').boundingBox();
 assert(directory.width>300,'directory gets additional horizontal room');assert(Math.abs(openDir.y-changeDir.y)<2&&Math.abs(openDir.y-retention.y)<2,'directory controls stay in one row');
 await page.evaluate(()=>Promise.allSettled(document.getAnimations().map(a=>a.finished)));await page.screenshot({path:path.join(out,'data-settings.png')});
 for(const width of [1000,760,540,360]){await page.setViewportSize({width,height:700});assert(await page.locator('#settings-storage').evaluate(el=>el.scrollWidth<=el.clientWidth),'data row fits narrow cards');assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));}
 await page.setViewportSize({width:1280,height:900});
 assert.equal(await page.locator('.about-links a').count(),3);await page.locator('[data-about-link="intro"]').click();assert(await page.evaluate(()=>requests.some(r=>r.method==='about_open'&&r.p.target==='intro')));
 await page.locator('#cfg-auto-update').uncheck();await page.waitForFunction(()=>fixture.config.auto_update===false);
 await page.locator('[data-update-check]').evaluate(el=>{el.click();el.click();});assert(await page.locator('[data-update-check]').isDisabled());
 await page.getByText('新版本已准备好',{exact:true}).waitFor();assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='update_check').length),1);
 await page.locator('[data-update-later]').click();await page.locator('#overlay').waitFor({state:'hidden'});await page.evaluate(()=>Promise.allSettled(document.getAnimations().map(a=>a.finished)));await page.screenshot({path:path.join(out,'about-update.png')});
 await page.locator('[data-update-check]').click();await page.locator('[data-update-confirm]').click();await page.waitForFunction(()=>requests.some(r=>r.method==='update_install'));
 await page.locator('#overlay').waitFor({state:'hidden'});await page.evaluate(async()=>{fixture.updates={status:'failed',error:'合成失败',updated_at:1700000000000};await refreshState(true);});
 assert(!(await page.locator('[data-update-check]').isDisabled()));assert.equal(await page.locator('#version-build-time').innerText(),buildLabel);assert(!(await page.locator('#settings-about').innerText()).includes('上次更新'));assert(!(await page.locator('#settings-about').innerText()).includes('安装于'));
 await page.locator('[data-page="logs"]').first().click();await page.locator('#log-search').waitFor();assert.equal(await page.locator('#page-title').innerText(),'系统日志');
 await page.locator('#log-search').fill('标题 合成');await page.waitForFunction(()=>requests.some(r=>r.method==='activity_query'&&r.p.query==='标题 合成'));
 await page.locator('[data-page="prompts"]').first().click();await page.locator('[data-prompt-section="memory"]').click();await page.locator('.memory-heading').first().waitFor();
 assert.equal(await page.locator('.memory-heading>.memory-caption').count(),2);for(const el of await page.locator('.memory-heading').all()){const title=await el.locator('h2').boundingBox(),caption=await el.locator('.memory-caption').boundingBox();assert(Math.abs(title.y-caption.y)<10);}
 assert((await page.locator('.memory-heading').last().innerText()).includes('操作记录'));await page.evaluate(()=>Promise.allSettled(document.getAnimations().map(a=>a.finished)));await page.screenshot({path:path.join(out,'memory-subtitles.png')});
 await page.setViewportSize({width:760,height:700});assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));assert.deepEqual(errors,[]);
 console.log('PASS stable task identifiers; searchable/sortable scoped history; about links; update switch/manual loading/dedup/ready confirmation/immutable build time; fuzzy log request; inline memory subtitles. '+out);
 }finally{await browser.close();}})().catch(e=>{console.error(e);process.exitCode=1});
