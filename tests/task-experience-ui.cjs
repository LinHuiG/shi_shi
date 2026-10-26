// SS-207–SS-212, synthetic IPC only. Exercises production components and shell.
const fs=require('fs'),path=require('path'),assert=require('assert'),{chromium}=require('playwright');
const at=Date.parse('2026-10-26T08:00:00+08:00');
const out=fs.mkdtempSync(path.join(require('os').tmpdir(),'shishi-task-experience-'));
const task=(id,extra={})=>({id,number:1,title:'合成事项 '+id,description:'保留事项描述及外围操作。',status:'待处理',today:true,created:at,updated:at,evidence_count:80,change_count:65,...extra});
const fixture={config:{enabled:false,auto_ai:false,leaders:[],interval_minutes:5,daily_calls:40,work_start:'09:00',work_end:'18:00'},tasks:[task('one'),task('two',{number:2}),task('done',{status:'已完成'}),task('deferred',{deferred:true}),task('archived',{archived:true}),task('dismissed',{status:'无需处理'}),task('cancelled',{status:'已取消'})],handling:[{id:'run',task_id:'one',thread_id:'thread',transport:'desktop',connection:{driver:'codex',name:'合成接入'},project_name:'合成项目',model:'synthetic-model',status:'inProgress',rounds:[{id:'first',question:'先检查合成数据',steps:[{id:'user',kind:'userMessage',text:'先检查合成数据'},{id:'assistant',kind:'agentMessage',text:'已确认第一轮结论'}],conclusion:'已确认第一轮结论'},{id:'second',question:'继续核对',steps:[{id:'tool',kind:'commandExecution',text:'读取合成配置\n'+Array.from({length:25},(_,i)=>'合成工具输出 '+i).join('\n')},{id:'answer',kind:'agentMessage',text:Array.from({length:30},(_,i)=>'正在核对合成资料 '+i).join('\n')}]}],requests:[]}],schedules:[{id:'s',title:'合成定时检查',description:'保留现有卡片内容',enabled:true,frequency:'daily',weekdays:[],time:'08:40',tags:[]}],messages:[],reports:[],suggestions:[],audit:[],activity:[],chat_sources:[],usage:{},task_history_revision:[1,65],messages_revision:[80,1]};
const mock=`window.fixture=${JSON.stringify(fixture)};window.requests=[];window.recordDelay=5;window.failRecords=false;window.failSchedule=false;window.scheduleDelay=80;
window.recordRows=Array.from({length:80},(_,i)=>({id:'m'+i,text:'合成消息 '+i,sender:i===3?'特定发送人':'合成发送人',source_name:'合成来源',group:'合成会话',time:${at}+i*60000}));
window.changeRowsFixture=Array.from({length:65},(_,i)=>({index:i,time:${at}+i*60000,kind:'manual_edit',detail:'合成修改 '+i,before:{description:'原描述'},after:{description:'新描述 '+i}}));
window.ipc={postMessage(raw){const r=JSON.parse(raw),p=r.params;requests.push(r);setTimeout(()=>{let data=true,error='';
if(r.method==='state')data=fixture;
if(['evidence_query','task_changes'].includes(r.method)){
 if(failRecords)error='合成读取失败';
 const all=p.id==='one'?(r.method==='evidence_query'?recordRows:changeRowsFixture):[],terms=p.query.toLowerCase().split(/\\s+/).filter(Boolean),rows=all.filter(m=>(p.since==null||m.time>=p.since)&&(p.until==null||m.time<=p.until)&&terms.every(q=>[m.text,m.detail,m.sender,m.group,m.source_name].join(' ').toLowerCase().includes(q))).sort((a,b)=>p.sort==='asc'?a.time-b.time:b.time-a.time),pages=Math.max(1,Math.ceil(rows.length/50)),page=Math.min(p.page,pages);
 data={rows:rows.slice((page-1)*50,page*50),all:all.length,total:rows.length,page,pages};
}
if(r.method==='schedule_edit'){if(failSchedule)error='合成保存失败';else fixture.schedules[0]=p.schedule;}
if(r.method==='handling_followup'){fixture.handling[0].rounds.push({id:'followup',question:p.text,steps:[]});}
receive({id:r.id,ok:!error,data,error});},['evidence_query','task_changes'].includes(r.method)?recordDelay:r.method==='schedule_edit'?scheduleDelay:5);}};`;
(async()=>{
 const browser=await chromium.launch({headless:true,executablePath:['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe','C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync),args:['--disable-background-networking']});
 try {
  const page=await browser.newPage({viewport:{width:1360,height:1000}}),errors=[];
  page.on('pageerror',e=>errors.push(e.message));page.setDefaultTimeout(10000);
  await page.setContent(require('./ui-bundle.cjs')().replace('<script>','<script>'+mock));

  assert.equal(await page.locator('.task-row').count(),2);assert.equal((await page.locator('[data-view="pending"]').innerText()).replace(/\s/g,''),'待处理2');
  assert.equal(await page.locator('[data-recent],.recent-item,#f-next,[data-action="prompt"]').count(),0);
  for(const name of ['编辑','合并'])assert(await page.locator('.detail-actions').getByText(name,{exact:true}).isVisible());
  await page.locator('[data-handle="start"]').click();await page.locator('#handling-question').waitFor();
  assert.equal(await page.locator('[data-handling-reading-toggle]').count(),0);assert.equal(await page.locator('.handling-user-message').count(),2);
  assert.equal(await page.locator('[data-handling-stream]').getByText('已确认第一轮结论',{exact:true}).count(),1);
  assert.equal(await page.locator('.handling-stream-detail').count(),1);
  await page.locator('.handling-stream-detail summary').click();assert(await page.locator('.handling-stream-detail').evaluate(e=>e.open));
  const stream=page.locator('[data-handling-stream]');
  await stream.evaluate(el=>el.scrollTop=0);await page.waitForTimeout(60);
  await page.evaluate(()=>{fixture.handling[0].rounds[1].steps[1].text+='\n最新完整文本';state.handling=structuredClone(fixture.handling);updateHandlingLive();});
  assert((await stream.innerText()).includes('最新完整文本'));assert.equal(await stream.evaluate(el=>el.scrollTop),0);
  assert(await page.locator('[data-handling-latest]').isVisible());
  await page.evaluate(()=>{const body=document.querySelector('[data-stream-kind="agentMessage"] .handling-stream-text'),range=document.createRange();range.selectNodeContents(body);getSelection().removeAllRanges();getSelection().addRange(range);fixture.handling[0].rounds[0].steps[1].text='选择期间不替换';state.handling=structuredClone(fixture.handling);updateHandlingLive();});
  assert((await stream.innerText()).includes('已确认第一轮结论'));
  await page.evaluate(()=>getSelection().removeAllRanges());await page.waitForFunction(()=>document.querySelector('[data-handling-stream]').textContent.includes('选择期间不替换'));
  const field=page.locator('#handling-question');await field.fill('合成追问');await field.press('Control+Enter');assert.equal(await field.inputValue(),'合成追问\n');
  await field.dispatchEvent('keydown',{key:'Enter',isComposing:true,keyCode:229,bubbles:true});assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='handling_followup').length),0);
  await page.locator('[data-handle="send"]').click();await page.waitForFunction(()=>document.querySelector('#handling-question').value==='');assert.equal(await field.count(),1);
  await page.locator('.handling-more>summary').click();assert(await page.locator('[data-handle="sync"]').isVisible());
  assert((await page.locator('[data-handle="terminate"]').getAttribute('title')).includes('Codex 继续运行'));await page.locator('.handling-more>summary').click();
  assert.deepEqual(await page.locator('.handling-turn-label').allTextContents(),['第 1 轮','上一轮','本轮']);
  async function assertDocked(){
    const geometry=await page.evaluate(()=>{
      const pane=document.querySelector('.handling-conversation').getBoundingClientRect(),dock=document.querySelector('#handling-dock').getBoundingClientRect(),read=document.querySelector('.handling-stream').getBoundingClientRect(),input=document.querySelector('.handling-composer').getBoundingClientRect();
      return {bottom:Math.abs(dock.bottom-pane.bottom),readGap:Math.abs(read.bottom-dock.top),inside:input.top>=dock.top&&input.bottom<=dock.bottom&&input.left>=pane.left&&input.right<=pane.right};
    });
    assert(geometry.bottom<1&&geometry.readGap<1&&geometry.inside,'composer is outside scrolling content, fixed to the column bottom: '+JSON.stringify(geometry));
  }
  for(const [width,height] of [[1360,1000],[1050,760],[900,620]]){
    await page.setViewportSize({width,height});await assertDocked();
    const top=await page.locator('.handling-composer').boundingBox();
    for(const scroll of [0,240,100000]){await stream.evaluate((el,y)=>el.scrollTop=y,scroll);await assertDocked();assert(Math.abs((await page.locator('.handling-composer').boundingBox()).y-top.y)<1);}
  }
  await page.setViewportSize({width:1360,height:1000});
  await page.evaluate(()=>{fixture.tasks[0].description='合成长说明\n'.repeat(40);fixture.handling[0].error='合成同步错误';fixture.handling[0].waiting_on='sync';});
  await page.evaluate(()=>refreshState(true));await assertDocked();
  assert((await page.locator('.handling-state-label').innerText()).includes('同步暂不可用'));
  await page.evaluate(()=>{fixture.tasks[0].description='保留事项描述及外围操作。';fixture.handling[0].error='';fixture.handling[0].waiting_on='';});
  await page.evaluate(()=>refreshState(true));await assertDocked();
  const alignment=await page.locator('.handling-heading').evaluate(el=>{
    const title=el.querySelector('h3').getBoundingClientRect(),open=el.querySelector('.handling-open').getBoundingClientRect(),more=el.querySelector('.handling-more').getBoundingClientRect();
    return open.left>title.right&&open.right<=more.left&&Math.abs(open.top-more.top)<8;
  });assert(alignment,'open Codex belongs to the right header actions');
  assert.equal(await page.locator('.handling-conversation').evaluate(el=>getComputedStyle(el).backgroundColor),'rgb(25, 25, 25)');
  assert.equal(await page.locator('.handling-composer').evaluate(el=>getComputedStyle(el).backgroundColor),'rgb(51, 51, 51)');
  await page.waitForTimeout(250);await page.screenshot({path:path.join(out,'conversation.png')});

  await page.locator('[data-handling-close]').click();
  const open=kind=>page.locator(`[data-task-record="${kind}"]`).click();
  await open('evidence');await page.waitForFunction(()=>document.querySelectorAll('.record-message').length===50);
  assert(await page.locator('#overlay').isHidden());assert.equal(await page.locator('#task-record-window').getAttribute('aria-modal'),'false');
  const header=await page.locator('.task-record-tools').boundingBox();
  await page.locator('.task-record-content').evaluate(el=>el.scrollTop=500);
  assert(Math.abs((await page.locator('.task-record-tools').boundingBox()).y-header.y)<1);
  await page.keyboard.press('Escape');await open('evidence');await page.waitForFunction(()=>document.querySelectorAll('.record-message').length===50);
  assert.equal(await page.locator('.task-record-content').evaluate(el=>el.scrollTop),500);
  await page.evaluate(async()=>{
    recordDelay=150;
    const refresh=loadTaskRecord();
    document.querySelector('.task-record-content').scrollTop=850;
    await refresh;recordDelay=5;
  });
  assert.equal(await page.locator('.task-record-content').evaluate(el=>el.scrollTop),850,'an in-flight refresh must retain the latest reader position');
  await page.locator('#task-record-search').fill('特定发送人');await page.waitForFunction(()=>document.querySelectorAll('.record-message').length===1);
  await page.locator('#task-record-search').fill('');await page.waitForFunction(()=>document.querySelectorAll('.record-message').length===50);
  await page.locator('[data-record-sort]').click();await page.waitForFunction(()=>document.querySelector('.record-message').textContent.includes('合成消息 0'));
  await page.locator('.record-range summary').click();await page.locator('#record-start').fill('2026-10-26T08:02');await page.locator('#record-end').fill('2026-10-26T08:03');await page.locator('[data-record-range-apply]').click();await page.waitForFunction(()=>document.querySelectorAll('.record-message').length===2);
  assert.deepEqual(await page.evaluate(()=>{const p=requests.filter(r=>r.method==='evidence_query').at(-1).params;return [p.since,p.until];}),[at+120000,at+239999]);
  await page.locator('.record-range summary').click();await page.locator('#record-start').fill('2026-10-27T00:00');await page.locator('[data-record-range-apply]').click();assert((await page.locator('#record-range-error').innerText()).includes('晚于'));
  await page.locator('[data-record-range-clear]').click();await page.waitForFunction(()=>document.querySelectorAll('.record-message').length===50);
  await page.locator('[data-record-page="2"]').click();await page.waitForFunction(()=>document.querySelectorAll('.record-message').length===30);
  // A drag that starts inside remains open even when released outside.
  await page.locator('.task-record-content').dispatchEvent('pointerdown',{pointerId:1,clientX:500,clientY:300,bubbles:true});await page.locator('#page-title').dispatchEvent('pointerup',{pointerId:1,clientX:100,clientY:50,bubbles:true});assert.equal(await page.locator('#task-record-window').count(),1);
  await page.evaluate(()=>window.dispatchEvent(new Event('blur')));assert.equal(await page.locator('#task-record-window').count(),1);
  await page.screenshot({path:path.join(out,'evidence.png')});
  await page.locator('[data-record-close]').click();await open('changes');await page.waitForFunction(()=>document.querySelectorAll('#task-record-window .event').length===50);
  await page.locator('#task-record-window .change-details summary').first().click();assert(await page.locator('#task-record-window .change-details').first().evaluate(el=>el.open));
  await page.waitForTimeout(200);
  await page.evaluate(async()=>{
    recordDelay=150;
    const refresh=loadTaskRecord(),list=document.querySelector('.task-record-content'),details=list.querySelectorAll('details');
    details[0].open=false;details[1].open=true;list.scrollTop=350;
    await refresh;recordDelay=5;
  });
  assert.equal(await page.locator('.task-record-content').evaluate(el=>el.scrollTop),350);
  assert(!(await page.locator('#task-record-window .change-details').nth(0).evaluate(el=>el.open)));
  assert(await page.locator('#task-record-window .change-details').nth(1).evaluate(el=>el.open),'a refresh must not undo a disclosure opened while reading');
  await page.locator('#task-record-search').fill('合成修改 64');await page.waitForFunction(()=>document.querySelectorAll('#task-record-window .event').length===1);
  assert(await page.locator('#task-record-window .event-heading').evaluate(el=>el.getBoundingClientRect().top>=el.closest('.task-record-content').getBoundingClientRect().top),'filtered row headings remain below the fixed toolbar');
  // A background revision during a debounced query must not restore the old
  // list's offset or details to the new results.
  await page.locator('#task-record-search').fill('');
  await page.evaluate(()=>loadTaskRecord());
  await page.waitForFunction(()=>document.querySelectorAll('#task-record-window .event').length===50);
  assert.equal(await page.locator('.task-record-content').evaluate(el=>el.scrollTop),0);
  assert.equal(await page.locator('#task-record-window details.change-details[open]').count(),0);
  await page.screenshot({path:path.join(out,'timeline.png')});
  await page.evaluate(()=>{recordDelay=150;});await page.locator('#task-record-search').fill('');await page.waitForTimeout(200);await page.locator('#page-title').click();await page.waitForTimeout(200);assert.equal(await page.locator('#task-record-window').count(),0);
  await page.evaluate(()=>{recordDelay=5;failRecords=true;});await open('changes');await page.locator('[data-record-retry]').waitFor();await page.evaluate(()=>failRecords=false);await page.locator('[data-record-retry]').click();await page.waitForFunction(()=>document.querySelectorAll('#task-record-window .event').length===50);
  await page.locator('[data-record-close]').click();await page.evaluate(()=>{selected='two';renderDetail();});await open('changes');await page.getByText('暂无保留的记录。',{exact:true}).waitFor();await page.keyboard.press('Escape');
  await page.evaluate(()=>{selected='one';renderDetail();});await open('changes');await page.waitForFunction(()=>document.querySelectorAll('#task-record-window .event').length===50);
  for(const width of [760,360]){
    await page.setViewportSize({width,height:760});
    // Viewport resizing can finish before the page's resize handler runs.
    await page.waitForFunction(()=>{const r=document.querySelector('#task-record-window').getBoundingClientRect();return r.left>=0&&r.right<=innerWidth;});
    assert(await page.locator('#task-record-window').evaluate(el=>el.getBoundingClientRect().left>=0&&el.getBoundingClientRect().right<=innerWidth));
    assert(await page.locator('.task-record-tools').isVisible());
  }
  await page.emulateMedia({reducedMotion:'reduce'});await page.keyboard.press('Escape');await open('evidence');assert.equal(await page.locator('#task-record-window').evaluate(el=>el.getAnimations().length),0);await page.keyboard.press('Escape');
  await page.setViewportSize({width:1360,height:1000});await page.locator('[data-page="schedules"]').first().click();await page.locator('.schedule-switch').waitFor();
  assert.equal(await page.locator('.schedule-heading>.badge,.schedule-actions>[data-schedule-toggle]').count(),0);
  assert.deepEqual(await page.locator('.schedule-switch').evaluate(el=>[el.offsetWidth,el.offsetHeight]),[26,15]);
  await page.evaluate(()=>failSchedule=true);await page.locator('.schedule-switch').click();await page.waitForFunction(()=>document.querySelector('.schedule-save-error'));
  assert.equal(await page.locator('.schedule-switch').getAttribute('aria-checked'),'true');await page.evaluate(()=>failSchedule=false);await page.locator('.schedule-save-error button').click();await page.waitForFunction(()=>!fixture.schedules[0].enabled&&!scheduleSaving.size);
  assert.equal(await page.locator('.schedule-switch').getAttribute('aria-checked'),'false');assert((await page.locator('.schedule-card').innerText()).includes('暂停期间不生成事项'));
  await page.screenshot({path:path.join(out,'schedule.png')});
  await page.locator('[data-page="tasks"]').first().click();
  await page.evaluate(async()=>{
    fixture.handling[0].effort='medium';fixture.handling[0].rounds=[
      {id:'preview-previous',status:'completed',conclusion:'已完成合成配置核对，等待确认后继续。'},
      {id:'preview-current',question:'先检查连接，暂时不要修改配置。',status:'inProgress',steps:[
        {id:'preview-tool',kind:'webSearch',text:'已检查合成应用入口'},
        {id:'preview-answer',kind:'agentMessage',text:'\n\n会先核对**连接路径和依赖地址**，确认原因后再给出修改建议。\n\n'},
        {id:'preview-command',kind:'commandExecution',text:'D:/fixture/tools/runtime.exe check --offline\n合成连接检查已完成'},
        {id:'preview-file',kind:'fileChange',text:'D:/fixture/docs/checks.md\n+ 合成检查记录'},
        {id:'preview-last',kind:'agentMessage',text:'入口可访问，正在继续核对依赖服务的访问方向与配置。'}
      ]}
    ];await refreshState(true);openHandlingWindow('one');document.activeElement?.blur();
    const output=document.querySelector('[data-handling-stream]');pauseHandlingStream(output,handlingStreams.get(output));output.scrollTop=0;
  });
  const density=await page.evaluate(()=>{
    const command=document.querySelector('[data-stream-kind="commandExecution"]'),file=document.querySelector('[data-stream-kind="fileChange"]'),a=command.querySelector('summary').getBoundingClientRect(),b=file.querySelector('summary').getBoundingClientRect();
    return {model:parseFloat(getComputedStyle(document.querySelector('.handling-composer-model')).fontSize),gap:b.top-a.bottom,body:document.querySelector('[data-stream-entry="preview-current:step:preview-answer"] .handling-stream-text').innerText};
  });
  assert(density.model>=13,'model metadata must remain readable');assert(density.gap>=0&&density.gap<=10,'consecutive tools must not leave oversized blank rows');assert.equal(density.body,density.body.trim());
  assert.equal(await page.locator('[data-stream-kind="commandExecution"] .handling-tool-label').innerText(),'执行命令');
  assert.equal(await page.locator('[data-stream-kind="fileChange"] .handling-tool-label').innerText(),'文件变更 · checks.md');
  assert((await page.locator('[data-stream-kind="commandExecution"] .handling-stream-text').textContent()).includes('D:/fixture/tools/runtime.exe'));
  assert.equal(await page.locator('[data-stream-entry="preview-current:step:preview-answer"] strong').innerText(),'连接路径和依赖地址');
  assert.equal(await page.locator('#handling-window [data-handle="history"]').count(),0);
  await assertDocked();await page.locator('#toast').waitFor({state:'hidden',timeout:8000});await page.waitForTimeout(250);await page.screenshot({path:path.join(out,'conversation.png')});
  assert.deepEqual(errors,[]);
  console.log('PASS SS-207–SS-212 conversation, scope, popup lifecycle, filters, bounds, request races, reduced motion and schedule rollback. '+out);
 }finally{await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1;});
