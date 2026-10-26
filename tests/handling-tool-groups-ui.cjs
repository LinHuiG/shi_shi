const fs=require('fs'),path=require('path'),assert=require('node:assert/strict'),{chromium}=require('playwright');
(async()=>{const browser=await chromium.launch({headless:true,executablePath:['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe','C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync),args:['--disable-background-networking']});try{
 const page=await browser.newPage({viewport:{width:1100,height:800}}),errors=[];page.on('pageerror',e=>errors.push(e.message));page.setDefaultTimeout(8000);
 await page.setContent(require('./handling-window-fixture.cjs')());await page.locator('[data-handle="start"]').click();
 await page.evaluate(async()=>{
  fixture.handling[0].rounds[0].steps=[{id:'before',kind:'agentMessage',text:'合成前文\n'.repeat(55)},
   {id:'cmd1',kind:'commandExecution',text:'"D:/fixture/tools/runtime.exe" check --offline\n'+'第一条合成输出\n'.repeat(32)},
   {id:'tail',kind:'agentMessage',text:'本轮记录已同步。'}];await refreshState(true);
 });
 const stream=page.locator('[data-handling-stream]'),group=page.locator('.handling-tool-group'),child=page.locator('[data-stream-entry="turn:step:cmd1"]'),settle=()=>page.waitForTimeout(230);
 assert.equal(await page.locator('.handling-state-label').count(),0);
 await child.locator('summary').evaluate(el=>el.click());await settle();
 await page.evaluate(async()=>{window.firstTool=document.querySelector('[data-stream-entry="turn:step:cmd1"]');fixture.handling[0].rounds[0].steps.splice(2,0,{id:'cmd2',kind:'commandExecution',text:'python check.py\n第二条输出'});await refreshState(true);});await settle();
 assert.equal(await group.count(),1);assert(await group.evaluate(el=>el.open));assert(await child.evaluate(el=>el===firstTool&&el.open));
 assert.equal(await group.locator(':scope>summary').innerText(),'执行命令 · 2\n›');
 assert.equal(await child.locator('.handling-tool-label').innerText(),'runtime.exe check --offline');
 assert((await child.locator('.handling-stream-text').textContent()).includes('D:/fixture/tools/runtime.exe'));
 assert.equal(await page.locator('[data-stream-entry="turn:step:cmd2"] .handling-tool-chevron').evaluate(el=>getComputedStyle(el).transform),'none','a closed child must not inherit its parent chevron rotation');
 await group.locator(':scope>summary').evaluate(el=>el.click());await settle();await page.locator('[data-handling-close]').click();await page.locator('[data-handle="start"]').click();
 assert(!(await group.evaluate(el=>el.open)));assert(await child.evaluate(el=>el.open),'inner state survives closing the outer group and window');
 const groupSelector='.handling-tool-group',childSelector='[data-stream-entry="turn:step:cmd1"]';
 async function sample(selector){return page.evaluate(selector=>new Promise(resolve=>{
  const output=document.querySelector('[data-handling-stream]'),el=document.querySelector(selector),rows=[],start=performance.now();el.querySelector(':scope>summary').click();
  const frame=()=>{rows.push({top:output.scrollTop,gap:output.scrollHeight-output.clientHeight-output.scrollTop});if(performance.now()-start<230)requestAnimationFrame(frame);else resolve(rows);};requestAnimationFrame(frame);
 }),selector);}
 for(const reducedMotion of ['no-preference','reduce']){
  await page.emulateMedia({reducedMotion});await page.evaluate(()=>{document.querySelector('.handling-tool-group').open=false;document.querySelector('[data-stream-entry="turn:step:cmd1"]').open=false;const el=document.querySelector('[data-handling-stream]');el.scrollTop=el.scrollHeight;});await settle();
  for(const selector of [groupSelector,childSelector,childSelector,groupSelector]){const frames=await sample(selector);assert(frames.every(r=>Math.abs(r.gap)<2),'both disclosure levels must stay at the bottom: '+JSON.stringify(frames));}
  await stream.evaluate(el=>el.scrollTop=120);await settle();
  for(const selector of [groupSelector,childSelector,childSelector,groupSelector]){const frames=await sample(selector);assert(frames.every(r=>Math.abs(r.top-120)<2),'away from bottom, expansion must retain reading position');}
 }
 await page.emulateMedia({reducedMotion:'no-preference'});await stream.evaluate(el=>el.scrollTop=el.scrollHeight);await settle();
 await page.evaluate(()=>{document.querySelector('.handling-tool-group>summary').click();setTimeout(async()=>{fixture.handling[0].rounds[0].steps.splice(-1,0,{id:'cmd3',kind:'commandExecution',text:'git diff --check\n第三条输出'});await refreshState(true);},40);});await page.waitForTimeout(300);
 assert(await group.evaluate(el=>el.open&&!el.style.height));assert.equal(await group.locator('.handling-tool-child').count(),3);assert(await stream.evaluate(el=>el.scrollHeight-el.clientHeight-el.scrollTop<2));
 await group.locator(':scope>summary').evaluate(el=>el.click());await settle();
 await page.evaluate(()=>{document.querySelector('.handling-tool-group>summary').click();setTimeout(()=>document.querySelector('[data-stream-entry="turn:step:cmd1"]>summary').click(),50);});await page.waitForTimeout(320);
 assert(await group.evaluate(el=>el.open&&!el.style.height));assert(await child.evaluate(el=>el.open&&!el.style.height));assert(await stream.evaluate(el=>el.scrollHeight-el.clientHeight-el.scrollTop<2));
 await page.evaluate(async()=>{fixture.handling[0].rounds[0].steps.push({id:'other',kind:'commandExecution',text:'echo separate'});fixture.handling[0].rounds.push({id:'next',steps:[{id:'next-command',kind:'commandExecution',text:'echo next round'}]});await refreshState(true);});
 assert.equal(await group.count(),1,'groups cannot cross a message or round boundary');assert.equal(await group.locator('.handling-tool-child').count(),3);
 // Paging may prepend records within an existing group. Preserve the outer
 // disclosure even when every inner record is closed and the first key moves.
 await page.evaluate(async()=>{
  window.existingGroup=document.querySelector('.handling-tool-group');existingGroup.open=true;
  existingGroup.querySelectorAll('details').forEach(el=>el.open=false);
  fixture.handling[0].rounds[0].steps.splice(1,0,{id:'older-command',kind:'commandExecution',text:'echo older history'});
  await refreshState(true);
 });await settle();
 assert(await group.evaluate(el=>el===existingGroup&&el.open),'prepending history must retain the open group node');
 assert.equal(await group.locator('.handling-tool-child').count(),4);
 await page.evaluate(async()=>{fixture.handling[0].rounds[0].steps.splice(1,1);await refreshState(true);});await settle();
 assert(await group.evaluate(el=>el===existingGroup&&el.open),'removing the first record must also preserve group state');
 await page.setViewportSize({width:600,height:580});await settle();assert(await page.locator('#handling-window').evaluate(el=>el.scrollWidth<=el.clientWidth));assert(await page.locator('#handling-question').isVisible());
 const out=fs.mkdtempSync(path.join(require('os').tmpdir(),'shishi-tool-groups-'));await page.setViewportSize({width:1000,height:780});
 await page.evaluate(async()=>{fixture.handling[0].rounds=[{id:'preview',question:'先核对合成配置，保留检查记录。',steps:[{id:'intro',kind:'agentMessage',text:'正在核对配置与检查结果。'},...Array.from({length:4},(_,i)=>({id:'command-'+i,kind:'commandExecution',text:['python inspect.py --offline','git diff --check','node verify.cjs','git status --short'][i]+'\n合成命令输出，完整记录可在这里查看。'})),{id:'end',kind:'agentMessage',text:'检查已完成，等待确认后继续。'}]}];await refreshState(true);const g=document.querySelector('.handling-tool-group');g.open=true;document.querySelector('[data-handling-stream]').scrollTop=0;document.activeElement?.blur();});await settle();
 await page.locator('#handling-window').screenshot({path:path.join(out,'tool-groups.png')});
 assert.deepEqual(errors,[]);console.log('PASS two-level tool grouping: identity, independent disclosure state, refresh, incremental grouping, boundaries, both scroll anchors, reduced motion and overlapping animations; '+out);
}finally{await browser.close();}})().catch(e=>{console.error(e);process.exitCode=1;});
