// Observe actual text/box coordinates over animation frames, not just classes.
const fs=require('fs'),path=require('path'),assert=require('assert'),{chromium}=require('playwright');
const out=fs.mkdtempSync(path.join(require('os').tmpdir(),'shishi-reading-motion-')),now=Date.now();
const text=Array.from({length:100},(_,i)=>`第 ${i} 行合成阅读内容，验证中心位置和完整上下文。`).join('\n');
const fixture={config:{enabled:false,auto_ai:false,leaders:[],interval_minutes:5,daily_calls:40,work_start:'09:00',work_end:'18:00'},usage:{},tasks:[{id:'task',title:'合成中心锚点',description:'合成说明。'.repeat(40),next_step:text,status:'待处理',today:true,created:now,updated:now}],handling:[{id:'run',task_id:'task',thread_id:'synthetic',status:'completed',rounds:[{id:'round',status:'completed',steps:[{id:'note',kind:'agentMessage',text}],conclusion:text}],requests:[]}],schedules:[],messages:[],audit:[],activity:[],reports:[],suggestions:[],chat_sources:[],pending:0,ai_busy:false};
const mock=`window.fixture=${JSON.stringify(fixture)};window.ipc={postMessage(raw){const r=JSON.parse(raw);setTimeout(()=>receive({id:r.id,ok:true,data:r.method==='state'?fixture:r.method==='evidence'?[]:true}),2)}};`;
(async()=>{const browser=await chromium.launch({headless:true,executablePath:['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe','C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync),args:['--disable-background-networking']});try{
 const page=await browser.newPage({viewport:{width:1280,height:900}}),errors=[];page.on('pageerror',e=>errors.push(e.message));await page.setContent(require('./ui-bundle.cjs')().replace('<script>','<script>'+mock));await page.locator('[data-handling-reading-toggle]:visible').first().waitFor();
 for(const selector of ['.handling-stream','.handling-conclusion']){
  const card=page.locator(selector),body=card.locator('.handling-reading-body');
  await body.evaluate(el=>{const pane=el.closest('.detail');pane.scrollTop+=el.getBoundingClientRect().top-pane.getBoundingClientRect().top-200;el.scrollTop=480;});await page.waitForTimeout(80);
  const measured=await body.evaluate(async body=>{
   const el=body.parentElement,button=el.querySelector('[data-handling-reading-toggle]'),pane=el.closest('.detail'),rect=body.getBoundingClientRect();
   const caret=document.caretRangeFromPoint(rect.left+rect.width/2,rect.top+rect.height/2),range=caret.cloneRange(),index=Math.min(range.startOffset,range.startContainer.length-1);range.setStart(range.startContainer,index);range.setEnd(range.startContainer,index+1);
   const y=()=>{const r=range.getBoundingClientRect();return (r.top+r.bottom)/2;},anchor=y(),start={top:rect.top,bottom:rect.bottom,outer:pane.scrollTop};
   async function toggle(refresh=false){button.click();const frames=[],started=performance.now();let i=0;do{await new Promise(requestAnimationFrame);if(refresh&&i++===2)await refreshState(true);const r=body.getBoundingClientRect();frames.push({height:r.height,top:r.top,bottom:r.bottom,y:y()});}while(performance.now()-started<200);return frames;}
   const expand=await toggle(true),expandedTop=body.getBoundingClientRect().top,expandedBottom=body.getBoundingClientRect().bottom,collapse=await toggle();
   const end={top:body.getBoundingClientRect().top,bottom:body.getBoundingClientRect().bottom,outer:pane.scrollTop};
   // Reverse an unfinished transition from its current geometry.
   button.click();await new Promise(requestAnimationFrame);await new Promise(requestAnimationFrame);const interrupted=body.clientHeight;button.click();const reversed=body.clientHeight;await new Promise(r=>setTimeout(r,180));
   return {anchor,start,end,expand,collapse,expandedTop,expandedBottom,interrupted,reversed,final:body.clientHeight,inline:body.style.height};
  });
  assert(new Set(measured.expand.map(f=>Math.round(f.height))).size>=3,'height expands over several frames');
  assert(new Set(measured.collapse.map(f=>Math.round(f.height))).size>=3,'height contracts over several frames');
  for(const frame of [...measured.expand,...measured.collapse])assert(Math.abs(frame.y-measured.anchor)<3,'the same central text stays in place throughout the transition');
  assert(measured.expandedTop<measured.start.top&&measured.expandedBottom>measured.start.bottom,'expansion opens above and below the anchor');
  assert(Math.abs(measured.start.top-measured.end.top)<3&&Math.abs(measured.start.outer-measured.end.outer)<3,'no-scroll round trip restores the same outer position');
  assert(Math.abs(measured.interrupted-measured.reversed)<2,'reversal starts from the current geometry');assert(measured.final<=120);assert.equal(measured.inline,'');
  // Continuous noncancelable edge events must not require a quiet interval.
  await body.evaluate(el=>{el.scrollTop=0;el.dispatchEvent(new WheelEvent('wheel',{bubbles:true,cancelable:true,deltaY:1}));el.scrollTop=el.scrollHeight;});await page.waitForTimeout(30);
  const edge=await body.evaluate(async el=>{const pane=el.closest('.detail'),before=pane.scrollTop,start=performance.now();let early=before;while(performance.now()-start<300){el.dispatchEvent(new WheelEvent('wheel',{bubbles:true,cancelable:false,deltaY:8}));if(performance.now()-start<150)early=pane.scrollTop;await new Promise(r=>setTimeout(r,6));}return {before,early,after:pane.scrollTop};});assert.equal(edge.early,edge.before);assert(edge.after>edge.before);
  // Send real browser input continuously, without an artificial quiet gap.
  // Remote input may cost ~65ms per event: allow reaching the edge plus the
  // existing 200ms hold. The synthetic check above verifies the delay itself.
  await body.evaluate(el=>{const pane=el.closest('.detail');pane.scrollTop+=el.getBoundingClientRect().top-pane.getBoundingClientRect().top-140;el.scrollTop=Math.max(0,el.scrollHeight-el.clientHeight-64);});await body.hover();
  const nativeBefore=await page.locator('.detail').evaluate(el=>el.scrollTop),started=Date.now();let nativeEarly=nativeBefore;
  do{await page.mouse.wheel(0,16);if(Date.now()-started<150)nativeEarly=await page.locator('.detail').evaluate(el=>el.scrollTop);}while(Date.now()-started<650);
  await page.waitForTimeout(40);assert.equal(nativeEarly,nativeBefore);assert(await page.locator('.detail').evaluate(el=>el.scrollTop)>nativeBefore,'continuous browser wheel input reaches the outer pane without stopping');
 }
 await page.emulateMedia({reducedMotion:'reduce'});await page.locator('.handling-conclusion [data-handling-reading-toggle]').click();assert(await page.locator('.conclusion-body').evaluate(el=>el.clientHeight===el.scrollHeight&&el.style.height===''));
 await page.setViewportSize({width:760,height:540});assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));await page.screenshot({path:path.join(out,'centered-reading.png')});assert.deepEqual(errors,[]);
 console.log('PASS measured center text over expand/contract frames; no-scroll round trip; refresh during transition; rapid reversal; noncancelable and native continuous edge handoff; reduced motion. '+out);
}finally{await browser.close();}})().catch(e=>{console.error(e);process.exitCode=1;});
