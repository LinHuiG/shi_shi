// Record-owned sticky titles and modal exit geometry, using only synthetic IPC.
const fs=require('fs'),path=require('path'),os=require('os'),assert=require('assert'),{chromium}=require('playwright');
const out=fs.mkdtempSync(path.join(os.tmpdir(),'shishi-reading-modal-')),now=Date.now();
const lines=(name,count)=>Array.from({length:count},(_,i)=>`${name} ${i}: synthetic content`).join('\n');
const fixture={version:'1.1.21',config:{enabled:false,auto_ai:false,leaders:[],interval_minutes:5,daily_calls:40,work_start:'09:00',work_end:'18:00'},usage:{day:'fixture',calls:0,input_tokens:0,output_tokens:0},tasks:[{id:'task',title:'Synthetic reading',description:'Synthetic description',next_step:lines('Next',50),today:true,status:'待处理',created:now,updated:now}],handling:[{id:'run',task_id:'task',thread_id:'synthetic',status:'completed',rounds:[{id:'round',status:'completed',steps:[{id:'input',kind:'userMessage',text:lines('Input',90)},{id:'progress',kind:'syntheticRecord',text:lines('Progress',90)},{id:'tool',kind:'functionCall',text:lines('Tool',60)}]}],requests:[]}],schedules:[],messages:[],audit:[],activity:[],reports:[],suggestions:[],chat_sources:[],pending:0,ai_busy:false};
const mock=`window.fixture=${JSON.stringify(fixture)};window.requests=[];window.ipc={postMessage(raw){const r=JSON.parse(raw);requests.push(r);let data=true;if(r.method==='state')data=fixture;if(r.method==='evidence')data=[];if(r.method==='activity_query')data={rows:[{kind:'ai_success',time:${now},detail:{kind:'analysis',model:'synthetic',request_id:'synthetic-trace'}}],total:1,pages:1,page:1};if(r.method==='ai_trace')data={input:${JSON.stringify(lines('Input',100))},output:${JSON.stringify(lines('Output',150))},schema:'{}',events:''};setTimeout(()=>receive({id:r.id,ok:true,data}),2)}};`;
(async()=>{const browser=await chromium.launch({headless:true,executablePath:['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe','C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync),args:['--disable-background-networking']});try{
 const page=await browser.newPage({viewport:{width:1280,height:900}}),errors=[];page.on('pageerror',e=>errors.push(e.message));
 await page.emulateMedia({reducedMotion:'reduce'});await page.setContent(require('./ui-bundle.cjs')().replace('<script>','<script>'+mock));await page.locator('.handling-stream-detail').first().waitFor();
 const card=page.locator('.handling-stream'),body=card.locator('.handling-reading-body'),pane=page.locator('.detail'),toggle=card.locator('[data-handling-reading-toggle]'),records=card.locator('.handling-stream-detail');
 const settle=()=>page.evaluate(()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve))));
 assert.equal(await card.locator(':scope > .handling-reading-title').count(),0);
 assert.deepEqual(await records.locator(':scope > summary').allTextContents(),['输入消息','处置进展','执行工具']);
 for(const size of [{width:1280,height:900},{width:760,height:760},{width:1280,height:540}]){
  await page.setViewportSize(size);await settle();if(await card.evaluate(el=>el.classList.contains('is-expanded')))await toggle.click();
  await records.evaluateAll(nodes=>nodes.forEach(el=>el.open=true));await settle();
  const compactWidth=await body.evaluate(el=>el.clientWidth);
  for(const index of [0,1,2]){
   const result=await records.nth(index).evaluate(el=>{const body=el.closest('.handling-reading-body'),pane=el.closest('.detail');pane.scrollTop+=body.getBoundingClientRect().top-pane.getBoundingClientRect().top-80;body.scrollTop+=el.getBoundingClientRect().top-body.getBoundingClientRect().top+90;const summary=el.querySelector('summary');return {title:summary.getBoundingClientRect().top,top:body.getBoundingClientRect().top};});
   assert(Math.abs(result.title-result.top)<2,'compact record title sits at the visible reading edge: '+JSON.stringify(result));
  }
  await toggle.click();await settle();assert.equal(await body.evaluate(el=>getComputedStyle(el).overflowY),'visible');assert.equal(await body.evaluate(el=>el.clientWidth),compactWidth,'expansion preserves text wrapping width');
  for(const index of [0,1,2]){
   await records.nth(index).evaluate(el=>{const pane=el.closest('.detail');pane.scrollTop+=el.getBoundingClientRect().top-pane.getBoundingClientRect().top+120;});await settle();
   const result=await records.nth(index).evaluate(el=>({title:el.querySelector('summary').getBoundingClientRect().top,top:el.closest('.detail').getBoundingClientRect().top}));
   assert(Math.abs(result.title-result.top)<2,'expanded record title sits at the main reading edge: '+JSON.stringify(result));
   if(index===1&&size.height===900)await pane.screenshot({path:path.join(out,'record-title-at-top.png')});
   await records.nth(index).evaluate(el=>{const pane=el.closest('.detail');pane.scrollTop+=el.getBoundingClientRect().bottom-pane.getBoundingClientRect().top+30;});await settle();
   assert(await records.nth(index).evaluate(el=>el.querySelector('summary').getBoundingClientRect().bottom<=el.closest('.detail').getBoundingClientRect().top+1),'title leaves with its own record');
  }
  await page.evaluate(()=>refreshState(true));await settle();assert.equal(await card.locator(':scope > .handling-reading-title').count(),0);assert(await records.nth(1).evaluate(el=>el.open));
 }
 await page.setViewportSize({width:1280,height:900});await page.emulateMedia({reducedMotion:'no-preference'});await page.locator('[data-page="logs"]').first().click();await page.locator('[data-trace]').waitFor();
 await page.locator('[data-trace]').click();await page.locator('#dialog h2').filter({hasText:'AI 调用详情'}).waitFor();await page.evaluate(()=>Promise.allSettled(document.getAnimations().filter(a=>a.effect.getTiming().iterations!==Infinity).map(a=>a.finished)));
 await page.locator('.trace-content > details').first().evaluate(el=>el.open=true);
 await page.evaluate(()=>{document.querySelector('#dialog').scrollTop=250;window.modalBefore={overlay:document.querySelector('#overlay'),dialog:document.querySelector('#dialog'),view:document.querySelector('#view'),calls:requests.length,viewScroll:document.querySelector('#view').scrollTop,scroll:document.querySelector('#dialog').scrollTop,rect:document.querySelector('#dialog').getBoundingClientRect().toJSON(),blur:getComputedStyle(document.querySelector('#overlay')).backdropFilter};window.modalRenderCount=0;const old=render;render=async(...args)=>{modalRenderCount++;return old(...args)};});
 const during=await page.evaluate(()=>{document.querySelector('#dialog [data-action="close"]').click();const overlay=document.querySelector('#overlay'),dialog=document.querySelector('#dialog');window.exitAnimation=modalClosing.animation;exitAnimation.pause();return {hidden:overlay.hidden,ghosts:document.querySelectorAll('[data-motion-ghost]').length,same:overlay===modalBefore.overlay&&dialog===modalBefore.dialog,rect:dialog.getBoundingClientRect().toJSON(),oldRect:modalBefore.rect,scroll:dialog.scrollTop,oldScroll:modalBefore.scroll,blur:getComputedStyle(overlay).backdropFilter,oldBlur:modalBefore.blur,calls:requests.length-modalBefore.calls,renders:modalRenderCount,locked:getComputedStyle(document.documentElement).overflowY,duration:exitAnimation.effect.getTiming().duration,transform:getComputedStyle(overlay).transform};});
 assert(!during.hidden);assert.equal(during.ghosts,0);assert(during.same);assert.deepEqual(during.rect,during.oldRect);assert.equal(during.scroll,during.oldScroll);assert.equal(during.blur,during.oldBlur);assert.equal(during.calls,0);assert.equal(during.renders,0);assert.equal(during.locked,'hidden');assert.equal(during.transform,'none');assert(during.duration<=160);
 await page.screenshot({path:path.join(out,'modal-close-original-layer.png')});await page.evaluate(()=>exitAnimation.finish());await page.locator('#overlay').waitFor({state:'hidden'});assert.equal(await page.evaluate(()=>document.querySelector('#view')===modalBefore.view&&document.querySelector('#view').scrollTop===modalBefore.viewScroll),true);
 // Each existing dismissal path must finish without an orphaned full-screen layer.
 for(const method of ['button','escape','backdrop']){
  await page.locator('[data-trace]').click();await page.locator('#dialog h2').waitFor();
  if(method==='button')await page.locator('#dialog [data-action="close"]').click();
  if(method==='escape')await page.keyboard.press('Escape');
  if(method==='backdrop')await page.locator('#overlay').click({position:{x:5,y:5}});
  await page.locator('#overlay').waitFor({state:'hidden'});assert.equal(await page.locator('[data-motion-ghost]').count(),0);
 }
 await page.evaluate(()=>{setModal('<h2>Immediate close</h2>');closeModal();});await page.locator('#overlay').waitFor({state:'hidden'});
 await page.evaluate(()=>{setModal('<h2>First dialog</h2>');closeModal();window.oldModalExit=modalClosing.animation;setModal('<h2>Reopened dialog</h2><div class="dialog-foot"><button data-action="close">关闭</button></div>');});
 await page.waitForTimeout(180);assert(!(await page.locator('#overlay').isHidden()));assert.equal(await page.locator('#dialog h2').innerText(),'Reopened dialog');assert.equal(await page.evaluate(()=>oldModalExit.playState),'idle');await page.keyboard.press('Escape');await page.locator('#overlay').waitFor({state:'hidden'});
 await page.evaluate(()=>{setModal('<h2>Pending operation</h2><button data-pending="true">Pending</button>');closeModal();});assert(!(await page.locator('#overlay').isHidden()));assert.equal(await page.evaluate(()=>modalClosing),null);await page.evaluate(()=>{delete document.querySelector('#dialog button').dataset.pending;closeModal();});await page.locator('#overlay').waitFor({state:'hidden'});
 await page.emulateMedia({reducedMotion:'reduce'});assert(await page.evaluate(()=>{setModal('<h2>Reduced motion</h2>');closeModal();return document.querySelector('#overlay').hidden&&!modalClosing&&!document.querySelector('[data-motion-ghost]');}));
 assert.deepEqual(errors,[]);console.log('PASS record-owned sticky titles at both visible edges, stable wrapping, record boundaries, three sizes; original-layer modal fade, geometry/blur/scroll lock, no ghost/request/render, all dismissal paths, interrupted reopen, pending protection and reduced motion. '+out);
}finally{await browser.close();}})().catch(e=>{console.error(e);process.exitCode=1;});
