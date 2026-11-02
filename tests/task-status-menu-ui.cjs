const fs=require('node:fs'),path=require('node:path'),os=require('node:os'),assert=require('node:assert/strict'),{withBrowser}=require('./ui-driver.cjs');
const out=fs.mkdtempSync(path.join(os.tmpdir(),'shishi-task-status-menu-'));
const now=Date.now(), fixture={tasks:[{id:'task',title:'Synthetic task',description:'Synthetic context',status:'待处理',due:new Date(now-3600000).toISOString(),created:now,updated:now,today:true,has_tool_calls:true,evidence:{direct:[],background:[],completion:[]}}],messages:[],schedules:[],handling:[],activity:[],audit:[],reports:[],suggestions:[],chat_sources:[],usage:{},config:{enabled:false,auto_ai:false,leaders:[],ai_connections:[],tools:[],interval_minutes:3,daily_calls:100,max_input_chars:80000,work_start:'08:00',work_end:'20:00'}};
const mock=`window.fixture=${JSON.stringify(fixture)};window.requests=[];window.ipc={postMessage(raw){const r=JSON.parse(raw),p=r.params;requests.push(r);setTimeout(()=>{let data=true;if(r.method==='state')data=structuredClone(fixture);if(r.method==='evidence')data=[];if(r.method==='action'){const task=fixture.tasks[0];if(p.action==='done')task.status='已完成';if(p.action==='reopen')task.status='待处理';if(p.action==='dismiss')task.status='无需处理';if(p.action==='defer'){task.status='待处理';task.deferred=true;task.deferred_until=Date.now()+86400000;}if(p.action==='start'){task.status='处理中';task.deferred=false;task.deferred_until=0;}task.updated++;}receive({id:r.id,ok:true,data});},5)}};`;
withBrowser(async(browser)=>{
    const page=await browser.newPage({viewport:{width:1280,height:900}}),errors=[];
    page.on('pageerror',error=>errors.push(error.message));
    await page.setContent(require('./ui-bundle.cjs')().replace('<script>','<script>'+mock));
    const trigger=page.locator('[data-task-status-trigger]'),menu=page.locator('.task-status-popover');
    const openMenu=async()=>{await page.locator('#task-search').hover();await trigger.hover();await menu.waitFor({state:'visible'});};
    const deadline=page.locator('.task-row[data-task="task"] > .tiny[style]');
    const checkDeadline=async(prefix,color)=>{
      await page.waitForFunction(([prefix,color])=>{
        const row=document.querySelector('.task-row[data-task="task"] > .tiny[style]');
        const detail=[...document.querySelectorAll('#task-detail section')].find(el=>el.querySelector('h3')?.textContent==='截止时间');
        return row?.textContent.startsWith(prefix)&&row.style.color===color&&detail?.textContent.includes(row.textContent);
      },[prefix,color]);
      assert((await deadline.innerText()).startsWith(prefix));
      if(prefix==='截止 ')assert(!/已逾期|即将截止/.test(await page.locator('#task-detail').innerText()));
    };
    await trigger.waitFor();
    await page.evaluate(()=>{tab="全部";renderTasks();});
    await checkDeadline('已逾期 · ','var(--red)');
    assert.equal(await trigger.innerText(),'开始处理');
    await page.evaluate(()=>{tab='全部';renderTasks();});
    assert.equal(await page.locator('.detail-actions > [data-defer-trigger],.detail-actions > [data-action="dismiss"],.detail-actions > [data-action="archive-note"]').count(),0);
    assert.equal(await page.locator('.detail-actions > [data-handle="start"]').count(),1);
    assert.equal(await page.locator('.detail-actions > [data-action="edit"],.detail-actions > [data-action="merge"],.detail-actions > [data-tool-run-history]').count(),3);
    assert.equal(await page.locator('[data-tool-run-history]').evaluate(el=>el.previousElementSibling.dataset.action),'merge');
    await trigger.hover();await menu.waitFor({state:'visible'});
    assert.equal(await trigger.getAttribute('aria-expanded'),'true');
    assert.deepEqual(await menu.locator('.task-status-menu > button').filter({hasNot:page.locator('.tiny')}).allTextContents(),['处理完成','无需处理','归档']);
    assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='action').length),0);
    assert(await menu.evaluate(el=>parseFloat(getComputedStyle(el).transitionDuration)>0));
    await page.waitForTimeout(170);
    const buttonBounds=await trigger.boundingBox(),menuBounds=await menu.boundingBox();
    await page.mouse.move(buttonBounds.x+30,buttonBounds.y+buttonBounds.height-1);
    await page.mouse.move(buttonBounds.x+30,menuBounds.y+3);
    await page.locator('[data-defer-trigger]').hover();assert(await menu.isVisible());
    await page.screenshot({path:path.join(out,'hover-menu.png')});
    await page.locator('#task-search').hover();await menu.waitFor({state:'hidden'});
    assert.equal(await trigger.getAttribute('aria-expanded'),'false');
    await trigger.focus();await trigger.press('ArrowDown');assert.equal(await page.evaluate(()=>document.activeElement.dataset.action),'done');
    await page.keyboard.press('ArrowDown');assert.equal(await page.evaluate(()=>document.activeElement.dataset.action),'defer');
    await page.keyboard.press('Escape');await menu.waitFor({state:'hidden'});assert(await trigger.evaluate(el=>el===document.activeElement));
    await trigger.press('ArrowUp');assert.equal(await page.evaluate(()=>document.activeElement.dataset.action),'archive-note');await page.keyboard.press('Escape');
    await trigger.click();await page.waitForFunction(()=>fixture.tasks[0].status==='处理中');await page.waitForFunction(()=>document.querySelector('[data-task-status-trigger]').textContent==='处理完成');
    assert.equal(await page.evaluate(()=>requests.filter(r=>r.method.startsWith('handling')).length),0);
    await trigger.click();await page.waitForFunction(()=>fixture.tasks[0].status==='已完成');await page.locator('[data-action="reopen"]').waitFor();assert.equal(await trigger.innerText(),'重新打开');
    await checkDeadline('截止 ','var(--sub)');
    await page.screenshot({path:path.join(out,'completed-with-deadline.png')});
    await trigger.click();await page.waitForFunction(()=>document.querySelector('[data-task-status-trigger]').textContent==='开始处理');
    await checkDeadline('已逾期 · ','var(--red)');
    await openMenu();await page.locator('[data-defer-trigger]').focus();await page.keyboard.press('ArrowRight');
    assert.equal(await page.evaluate(()=>document.activeElement.dataset.deferChoice),'hour',await page.evaluate(()=>JSON.stringify({active:document.activeElement.outerHTML,host:document.querySelector('[data-task-status]').outerHTML,refresh:!!refreshInFlight})));
    await page.keyboard.press('ArrowLeft');assert.equal(await page.locator('[data-defer-trigger]').getAttribute('aria-expanded'),'false');
    await page.locator('[data-defer-trigger]').click();await page.waitForFunction(()=>fixture.tasks[0].deferred);
    await trigger.click();await page.waitForFunction(()=>fixture.tasks[0].deferred===false);
    await openMenu();await page.locator('[data-action="archive-note"]').click();await page.locator('#archive-status').waitFor();assert(await page.locator('#archive-note').isVisible());await page.locator('#overlay [data-action="close"]').click();await page.locator('#overlay').waitFor({state:'hidden'});
    await openMenu();await page.locator('[data-action="dismiss"]').click();await page.waitForFunction(()=>fixture.tasks[0].status==='无需处理');
    await checkDeadline('截止 ','var(--sub)');
    assert(await page.evaluate(()=>requests.some(r=>r.method==='action'&&r.params.action==='dismiss')));
    await page.evaluate(async()=>{fixture.tasks[0].status='待处理';await refreshState(true);});
    for(const width of [1280,760,420,360]){
      await page.setViewportSize({width,height:900});await openMenu();await page.waitForTimeout(170);
      const bounds=await menu.boundingBox();assert(bounds.x>=0&&bounds.x+bounds.width<=width,JSON.stringify({width,bounds}));
      assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
      await page.screenshot({path:path.join(out,'menu-'+width+'.png')});
    }
    await page.evaluate(async()=>{fixture.tasks[0].status='待处理';fixture.tasks[0].updated++;await refreshState(true);});
    await page.locator('[data-action="edit"]').click();
    assert.equal(await page.locator('select#f-status').count(),0);
    await page.locator('[data-task-status-value="处理中"]').click();
    assert.equal(await page.locator('.task-status-choices [aria-checked="true"]').count(),1);
    assert.equal(await page.locator('#f-status').inputValue(),'处理中');
    await page.keyboard.press('ArrowRight');assert.equal(await page.locator('#f-status').inputValue(),'已完成');
    await page.locator('#f-status').evaluate(el=>el.parentElement.scrollIntoView({block:'center'}));await page.mouse.move(1,1);await page.waitForTimeout(180);
    await page.screenshot({path:path.join(out,'status-buttons.png')});
    await page.locator('#overlay [data-action="close"]').click();
    for(const width of [1280,760,420,360]) {
      await page.setViewportSize({width,height:900});await openMenu();await page.locator('[data-defer-trigger]').hover();
      const sub=page.locator('.task-defer-popover');await sub.waitFor({state:'visible'});await page.waitForTimeout(180);
      const bounds=await sub.boundingBox();assert(bounds.x>=0&&bounds.x+bounds.width<=width,JSON.stringify({width,bounds}));
      await page.screenshot({path:path.join(out,'deferral-'+width+'.png')});
    }
    const times=await page.evaluate(()=>[12,13,17].map(hour=>taskDeferralChoices(Date.parse(`2026-10-28T${hour}:00:00+08:00`)).map(x=>x.choice)));
    assert.deepEqual(times,[['hour','afternoon','evening','tomorrow','next_week'],['hour','evening','tomorrow','next_week'],['hour','tomorrow','next_week']]);
    await page.emulateMedia({reducedMotion:'reduce'});assert.equal(await menu.evaluate(el=>parseFloat(getComputedStyle(el).transitionDuration)),0);
    for(const hours of [-1,1])for(const status of ['待处理','处理中','已完成','无需处理','已取消'])for(const archived of [false,true]){
      await page.evaluate(async({status,archived,due})=>{
        Object.assign(fixture.tasks[0],{status,archived,due});fixture.tasks[0].updated++;
        tab=archived?'归档':'全部';await refreshState(true);
      },{status,archived,due:new Date(now+hours*3600000).toISOString()});
      const active=['待处理','处理中'].includes(status)&&!archived;
      await checkDeadline(active?(hours<0?'已逾期 · ':'即将截止 · '):'截止 ',active&&hours<0?'var(--red)':'var(--sub)');
    }
    await page.evaluate(async()=>{Object.assign(fixture.tasks[0],{status:'待处理',archived:false,due:''});fixture.tasks[0].updated++;tab='全部';await refreshState(true);});
    assert.equal(await deadline.innerText(),'');
    assert.equal(await page.locator('#task-detail section').filter({hasText:'截止时间'}).count(),0);
    await page.evaluate(async()=>{fixture.tasks[0].due='unscheduled';fixture.tasks[0].updated++;await refreshState(true);});
    await checkDeadline('unscheduled','var(--sub)');
    const filterResult=await page.evaluate(()=>{
      const base={status:'处理中',archived:false,merged_into:'',blocker:'',waiting_for:'',deferred:false};
      return [{},{blocker:'blocked'},{waiting_for:'reply'},{deferred_until:Date.now()+60000},{archived:true},{merged_into:'other'}].map(p=>taskScopeMatches({...base,...p},'处理中'));
    });assert.deepEqual(filterResult,[true,false,false,false,false,false]);
    const blocked=await page.evaluate(()=>{
      const base={status:'待处理',archived:false,merged_into:'',blocker:'',waiting_for:''};
      return [{},{waiting_for:'reply'},{blocker:'blocked'},{blocker:'blocked',waiting_for:'reply'},{waiting_for:'  '},{status:'处理中',waiting_for:'reply'},{status:'已完成',waiting_for:'reply'}].map(p=>taskQuickViewMatches({...base,...p},'blocked'));
    });assert.deepEqual(blocked,[false,true,true,true,false,true,false]);
    const sorted=await page.evaluate(()=>{
      const previous=state.tasks;const oldTab=tab;tab='全部';quickView='';query='';selectedTags={};selectedPriorities=[];
      state.tasks=[{id:'low-new',priority_rank:3,updated:30},{id:'high-old',priority_rank:0,updated:10},{id:'high-new',priority_rank:0,updated:20},{id:'no-level',priority_rank:10000,updated:99}].map(t=>({...t,status:'待处理',tags:[]}));
      const ids=visibleTasks().map(t=>t.id);state.tasks=previous;tab=oldTab;return ids;
    });assert.deepEqual(sorted,['high-new','high-old','low-new','no-level']);
    assert.deepEqual(errors,[]);console.log('PASS status actions, complete/reopen overdue text and color, pending/completed/dismissed/cancelled/archived deadline matrix, no deadline, keyboard, archive form, responsive theme and reduced motion. '+out);
  }).catch(error=>{console.error(error);process.exitCode=1;});
