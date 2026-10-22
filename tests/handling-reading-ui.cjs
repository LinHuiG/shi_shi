// Full reading-card disclosure, synthetic streaming and main-pane scrolling.
const fs=require('fs'),path=require('path'),assert=require('assert'),{chromium}=require('playwright');
const now=Date.now(),out=fs.mkdtempSync(path.join(require('os').tmpdir(),'shishi-reading-ui-'));
const lines=(label,count)=>Array.from({length:count},(_,i)=>`${label} ${i}：合成中文与 emoji 🚀。`).join('\n');
const round={id:'round',status:'inProgress',steps:[{id:'note',kind:'agentMessage',text:lines('进展',70)},{id:'command',kind:'commandExecution',text:lines('命令输出',90)}],conclusion:lines('结论',60)+'\n\n```text\n'+lines('代码',100)+'\n```'};
const fixture={config:{enabled:false,auto_ai:false,leaders:[],interval_minutes:5,daily_calls:40,work_start:'09:00',work_end:'18:00'},usage:{},tasks:[{id:'task',title:'合成阅读验收',description:'完整阅读当前处置内容。',next_step:lines('后续说明',40),status:'待处理',today:true,created:now,updated:now}],handling:[{id:'run',task_id:'task',thread_id:'synthetic',status:'inProgress',rounds:[round],requests:[]}],schedules:[],messages:[],audit:[],activity:[],reports:[],suggestions:[],chat_sources:[],pending:0,ai_busy:false};
const mock=`window.fixture=${JSON.stringify(fixture)};window.originalRound=structuredClone(fixture.handling[0].rounds[0]);window.ipc={postMessage(raw){const r=JSON.parse(raw);setTimeout(()=>receive({id:r.id,ok:true,data:r.method==='state'?fixture:r.method==='evidence'?[]:true}),2)}};`;
(async()=>{const browser=await chromium.launch({headless:true,executablePath:['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe','C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync),args:['--disable-background-networking']});try{
 const page=await browser.newPage({viewport:{width:1280,height:900}}),errors=[];page.on('pageerror',e=>errors.push(e.message));
 await page.setContent(require('./ui-bundle.cjs')().replace('<script>','<script>'+mock));await page.locator('[data-handling-reading-toggle]:visible').first().waitFor();
 const cards=page.locator('[data-handling-reading]'),pane=page.locator('.detail');assert.equal(await cards.count(),2);
 async function checkLayout(card,expanded){
  const result=await card.evaluate(el=>{const body=el.querySelector('.handling-reading-body'),footer=el.querySelector('.handling-reading-footer'),button=el.querySelector('[data-handling-reading-toggle]'),style=getComputedStyle(body);return {expanded:button.getAttribute('aria-expanded'),bottom:!!(body.compareDocumentPosition(footer)&Node.DOCUMENT_POSITION_FOLLOWING),complete:body.scrollHeight<=body.clientHeight+1,compact:body.clientHeight<=120,overflow:style.overflowY,limit:style.maxHeight,nested:[...body.querySelectorAll('*')].some(n=>/(auto|scroll)/.test(getComputedStyle(n).overflowY)&&n.scrollHeight>n.clientHeight+1)};});
  assert.equal(result.expanded,String(expanded));assert(result.bottom,'control follows the body in document order');
  if(expanded){assert(result.complete,'all current content fits without clipping');assert.equal(result.limit,'none');assert(!result.nested,'long Markdown code must also be fully expanded');}
  else {assert(result.compact);assert.equal(result.overflow,'auto');}
 }
 async function checkSticky(card){const footer=await card.locator('.handling-reading-footer').boundingBox(),outer=await pane.boundingBox();assert(footer.y>=outer.y&&footer.y+footer.height<=outer.y+outer.height+1,'expanded controls stay visible');assert(outer.y+outer.height-footer.y-footer.height<2,'controls float at the actual visible lower edge');}
 async function checkCompactAndAnchor(card){
  const body=card.locator('.handling-reading-body');
  for(const direction of [-1,1]){
   await body.evaluate((el,direction)=>{const pane=el.closest('.detail');pane.scrollTop+=el.getBoundingClientRect().top-pane.getBoundingClientRect().top-120;el.scrollTop=direction<0?0:el.scrollHeight;},direction);await page.waitForTimeout(80);
   await body.hover();const trace=await body.evaluate(async(el,direction)=>{const pane=el.closest('.detail'),before=pane.scrollTop,start=performance.now();let early=before;while(performance.now()-start<310){el.dispatchEvent(new WheelEvent('wheel',{bubbles:true,cancelable:true,deltaY:direction*18}));if(performance.now()-start<150)early=pane.scrollTop;await new Promise(r=>setTimeout(r,8));}return {before,early,after:pane.scrollTop};},direction);assert.equal(trace.early,trace.before,'boundary buffers the first 200ms');assert(direction>0?trace.after>trace.before:trace.after<trace.before,'continuous input chains without waiting for silence');
  }
  await body.scrollIntoViewIfNeeded();await body.evaluate(el=>el.scrollTop=160);await page.waitForTimeout(180);
  const before=await pane.evaluate(el=>el.scrollTop),innerBefore=await body.evaluate(el=>el.scrollTop);await body.hover();await page.mouse.wheel(0,60);await page.waitForTimeout(180);
  assert(await body.evaluate(el=>el.scrollTop)>innerBefore,'compact wheel reads inside the card');assert.equal(await pane.evaluate(el=>el.scrollTop),before);
  await body.evaluate(el=>el.scrollTop=260);await page.waitForTimeout(80);
  const anchor=await body.evaluate(el=>({top:el.firstElementChild.getBoundingClientRect().top,width:el.clientWidth}));
  await card.locator('[data-handling-reading-toggle]').click();await page.waitForTimeout(180);const after=await body.evaluate(el=>({top:el.firstElementChild.getBoundingClientRect().top,width:el.clientWidth,scroll:el.scrollTop}));
  assert(Math.abs(anchor.top-after.top)<2,'expansion keeps the same visible text position');assert.equal(anchor.width,after.width,'wrapping width stays stable');assert.equal(after.scroll,0);await checkLayout(card,true);await checkSticky(card);
 }
 async function checkWheel(card){
  await card.locator('.handling-reading-body').evaluate(el=>{const pane=el.closest('.detail');pane.scrollTop+=el.getBoundingClientRect().top-pane.getBoundingClientRect().top-80;});
  assert(await card.locator('.handling-reading-body').evaluate(el=>el.firstElementChild.getBoundingClientRect().top>=el.closest('.detail').getBoundingClientRect().top),'earlier content remains reachable above the expansion anchor');
  const rect=await pane.boundingBox();await page.mouse.move(rect.x+rect.width/2,rect.y+120);
  for(const direction of [1,-1]){const before=await pane.evaluate(el=>el.scrollTop);await page.mouse.wheel(0,direction*160);await page.waitForTimeout(180);const after=await pane.evaluate(el=>el.scrollTop);assert(direction>0?after>before:after<before,'wheel scrolls the main detail pane');}
 }
 for(const size of [{width:1280,height:900},{width:760,height:760},{width:1280,height:540}]){
  await page.setViewportSize(size);await page.waitForTimeout(180);
  for(const card of await cards.all()){
   await checkLayout(card,false);const toggle=card.locator('[data-handling-reading-toggle]');await checkCompactAndAnchor(card);await checkWheel(card);await checkSticky(card);
   await toggle.click();await page.waitForTimeout(180);await checkLayout(card,false);const rect=await card.boundingBox(),outer=await pane.boundingBox();assert(rect.y>=outer.y-1&&rect.y+rect.height<=outer.y+outer.height+1,'collapsed card is reachable after shrinking');
  }
  assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
 }
 await page.setViewportSize({width:1280,height:900});await page.emulateMedia({reducedMotion:'reduce'});
 for(const card of await cards.all())await card.locator('[data-handling-reading-toggle]').click();
 await page.locator('.handling-stream-detail > summary').click();assert.equal(await page.locator('.handling-stream-detail').getAttribute('open'),'');
 await page.evaluate(()=>{window.savedStream=document.querySelector('[data-handling-stream]');window.savedEntry=savedStream.firstElementChild;window.savedDetail=savedStream.querySelector('details');const pane=savedStream.closest('.detail');pane.scrollTop+=savedStream.getBoundingClientRect().top-pane.getBoundingClientRect().top+240;});
 await page.waitForTimeout(80);const before=await pane.evaluate(el=>el.scrollTop);
 await page.evaluate(async()=>{const r=fixture.handling[0].rounds[0];r.steps[0].text+='\n新增进展';r.conclusion+='\n\n新增结论';await refreshState(true);});await page.waitForTimeout(180);
 assert.equal(await pane.evaluate(el=>el.scrollTop),before);assert(await page.evaluate(()=>savedStream===document.querySelector('[data-handling-stream]')&&savedEntry===savedStream.firstElementChild&&savedDetail===savedStream.querySelector('details')&&savedDetail.open));
 for(const card of await cards.all())await checkLayout(card,true);
 await page.screenshot({path:path.join(out,'floating-progress.png')});
 await page.locator('[data-handling-latest]').click();await page.waitForTimeout(80);assert(await page.locator('[data-handling-stream]').evaluate(el=>el.getBoundingClientRect().bottom<=el.parentElement.querySelector('.handling-reading-footer').getBoundingClientRect().top),'last content is above the floating toolbar');
 await pane.evaluate(el=>el.scrollTop=el.scrollHeight);assert(await cards.evaluateAll(els=>els.every(el=>el.querySelector('.handling-reading-footer').getBoundingClientRect().bottom<=el.closest('.detail').getBoundingClientRect().top)),'toolbars leave the viewport with their own cards');
 await page.evaluate(async()=>{const run=fixture.handling[0];run.rounds.push({...structuredClone(originalRound),id:'second'});await refreshState(true);});
 for(const card of await cards.all())await checkLayout(card,false);
 // The first round's two independent choices survive leaving and returning.
 await page.evaluate(async()=>{fixture.handling[0].rounds.pop();await refreshState(true);});for(const card of await cards.all())await checkLayout(card,true);
 // Short/empty content and later growth must not require a reload to measure.
 await page.evaluate(async()=>{fixture.handling[0].rounds.push({id:'short',status:'inProgress',steps:[],conclusion:'简短结论'});await refreshState(true);});await page.waitForTimeout(80);
 assert.equal(await page.locator('[data-handling-reading-toggle]:visible').count(),0);
 await page.evaluate(async()=>{fixture.handling[0].rounds.at(-1).steps=[structuredClone(originalRound.steps[0])];fixture.handling[0].rounds.at(-1).conclusion=originalRound.conclusion;await refreshState(true);});await page.waitForTimeout(180);
 assert.equal(await page.locator('[data-handling-reading-toggle]:visible').count(),2);for(const card of await cards.all())await checkLayout(card,false);
 await page.screenshot({path:path.join(out,'collapsed.png')});
 // A compact synthetic visual shows both expanded cards and their bottom bars.
 await page.setViewportSize({width:1280,height:1050});await page.evaluate(async()=>{const r=fixture.handling[0].rounds.at(-1);r.id='visual';r.steps[0].text=originalRound.steps[0].text.split('\n').slice(0,9).join('\n');r.conclusion=originalRound.conclusion.split('\n').slice(0,9).join('\n');await refreshState(true);});await page.waitForTimeout(180);
 for(const card of await cards.all())await card.locator('[data-handling-reading-toggle]').click();await page.locator('.handling-panel').evaluate(el=>el.closest('.detail').scrollTop+=el.getBoundingClientRect().top-el.closest('.detail').getBoundingClientRect().top);
 await page.screenshot({path:path.join(out,'expanded.png')});assert.deepEqual(errors,[]);
 console.log('PASS matching floating bottom disclosures; compact wheel; expansion anchor and stable wrapping; unrestricted earlier/later prose and code; main-pane wheel; three window sizes; stable streaming/read position; per-round state; short/empty/growing content; reduced motion. '+out);
}finally{await browser.close();}})().catch(e=>{console.error(e);process.exitCode=1;});
