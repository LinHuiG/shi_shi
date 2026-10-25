const fs=require('node:fs'),path=require('node:path'),os=require('node:os'),assert=require('node:assert/strict'),{chromium}=require('playwright');
const out=fs.mkdtempSync(path.join(os.tmpdir(),'shishi-task-status-menu-'));
const now=Date.now(), fixture={tasks:[{id:'task',title:'Synthetic task',description:'Synthetic context',status:'待处理',created:now,updated:now,today:true,has_tool_calls:true,evidence:{direct:[],background:[],completion:[]}}],messages:[],schedules:[],handling:[],activity:[],audit:[],reports:[],suggestions:[],chat_sources:[],usage:{},config:{enabled:false,auto_ai:false,leaders:[],ai_connections:[],tools:[],interval_minutes:3,daily_calls:100,max_input_chars:80000,work_start:'08:00',work_end:'20:00'}};
const mock=`window.fixture=${JSON.stringify(fixture)};window.requests=[];window.ipc={postMessage(raw){const r=JSON.parse(raw),p=r.params;requests.push(r);setTimeout(()=>{let data=true;if(r.method==='state')data=structuredClone(fixture);if(r.method==='evidence')data=[];if(r.method==='action'){const task=fixture.tasks[0];if(p.action==='done')task.status='已完成';if(p.action==='reopen')task.status='待处理';if(p.action==='dismiss')task.status='无需处理';if(p.action==='tomorrow')task.deferred=true;if(p.action==='resume_today')task.deferred=false;task.updated++;}receive({id:r.id,ok:true,data});},5)}};`;
(async()=>{
  const browser=await chromium.launch({headless:true,executablePath:['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe','C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync),args:['--disable-background-networking']});
  try{
    const page=await browser.newPage({viewport:{width:1280,height:900}}),errors=[];
    page.on('pageerror',error=>errors.push(error.message));
    await page.setContent(require('./ui-bundle.cjs')().replace('<script>','<script>'+mock));
    const trigger=page.locator('[data-task-status-trigger]'),menu=page.locator('.task-status-popover');
    const openMenu=async()=>{await page.locator('#task-search').hover();await trigger.hover();await menu.waitFor({state:'visible'});};
    await trigger.waitFor();
    assert.equal(await trigger.innerText(),'标记完成');
    assert.equal(await page.locator('.detail-actions > [data-action="tomorrow"],.detail-actions > [data-action="dismiss"],.detail-actions > [data-action="archive-note"]').count(),0);
    assert.equal(await page.locator('.detail-actions > [data-handle="start"]').count(),1);
    assert.equal(await page.locator('.detail-actions > [data-action="edit"],.detail-actions > [data-action="merge"],.detail-actions > [data-tool-run-history]').count(),3);
    assert.equal(await page.locator('[data-tool-run-history]').evaluate(el=>el.previousElementSibling.dataset.action),'merge');
    await trigger.hover();await menu.waitFor({state:'visible'});
    assert.equal(await trigger.getAttribute('aria-expanded'),'true');
    assert.deepEqual(await menu.locator('[role="menuitem"]').allTextContents(),['无需处理','明日处理','归档']);
    assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='action').length),0);
    assert(await menu.evaluate(el=>parseFloat(getComputedStyle(el).transitionDuration)>0));
    await page.waitForTimeout(170);
    const buttonBounds=await trigger.boundingBox(),menuBounds=await menu.boundingBox();
    await page.mouse.move(buttonBounds.x+30,buttonBounds.y+buttonBounds.height-1);
    await page.mouse.move(buttonBounds.x+30,menuBounds.y+3);
    await page.locator('[data-action="tomorrow"]').hover();assert(await menu.isVisible());
    await page.screenshot({path:path.join(out,'hover-menu.png')});
    await page.locator('#task-search').hover();await menu.waitFor({state:'hidden'});
    assert.equal(await trigger.getAttribute('aria-expanded'),'false');
    await trigger.focus();await trigger.press('ArrowDown');assert.equal(await page.evaluate(()=>document.activeElement.dataset.action),'dismiss');
    await page.keyboard.press('ArrowDown');assert.equal(await page.evaluate(()=>document.activeElement.dataset.action),'tomorrow');
    await page.keyboard.press('Escape');await menu.waitFor({state:'hidden'});assert(await trigger.evaluate(el=>el===document.activeElement));
    await trigger.press('ArrowUp');assert.equal(await page.evaluate(()=>document.activeElement.dataset.action),'archive-note');await page.keyboard.press('Escape');
    await trigger.click();await page.waitForFunction(()=>fixture.tasks[0].status==='已完成');await page.locator('[data-action="reopen"]').waitFor();assert.equal(await trigger.innerText(),'重新打开');
    await trigger.click();await page.locator('[data-action="done"]').waitFor();
    await openMenu();await page.locator('[data-action="tomorrow"]').click();await page.waitForFunction(()=>fixture.tasks[0].deferred);
    await openMenu();await page.locator('[data-action="resume-today"]').click();await page.waitForFunction(()=>fixture.tasks[0].deferred===false);
    await openMenu();await page.locator('[data-action="archive-note"]').click();await page.locator('#archive-status').waitFor();assert(await page.locator('#archive-note').isVisible());await page.locator('#overlay [data-action="close"]').click();await page.locator('#overlay').waitFor({state:'hidden'});
    await openMenu();await page.locator('[data-action="dismiss"]').click();await page.waitForFunction(()=>fixture.tasks[0].status==='无需处理');
    assert(await page.evaluate(()=>requests.some(r=>r.method==='action'&&r.params.action==='dismiss')));
    await page.evaluate(async()=>{fixture.tasks[0].status='待处理';await refreshState(true);});
    for(const width of [1280,760,420,360]){
      await page.setViewportSize({width,height:900});await openMenu();await page.waitForTimeout(170);
      const bounds=await menu.boundingBox();assert(bounds.x>=0&&bounds.x+bounds.width<=width,JSON.stringify({width,bounds}));
      assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
      await page.screenshot({path:path.join(out,'menu-'+width+'.png')});
    }
    await page.emulateMedia({reducedMotion:'reduce'});assert.equal(await menu.evaluate(el=>parseFloat(getComputedStyle(el).transitionDuration)),0);
    assert.deepEqual(errors,[]);console.log('PASS unchanged primary actions, hover bridge, both transitions, direct complete/reopen, keyboard, original status requests, archive form, responsive theme and reduced motion. '+out);
  }finally{await browser.close();}
})().catch(error=>{console.error(error);process.exitCode=1;});
