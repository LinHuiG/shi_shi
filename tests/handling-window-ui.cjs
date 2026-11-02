const assert=require('node:assert/strict'),{withBrowser}=require('./ui-driver.cjs');
const fixture=require('./handling-window-fixture.cjs');
withBrowser(async(browser)=>{
 async function scenario(name,check){
  const context=await browser.newContext({viewport:{width:1280,height:900}});
  try{
   const page=await context.newPage(),errors=[];
   page.on('pageerror',e=>errors.push(e.message));page.setDefaultTimeout(8000);
   await page.setContent(fixture());await page.locator('[data-handle="start"]').click();await page.locator('#handling-window').waitFor();
   await check(page,page.locator('[data-handling-stream]'),page.locator('.handling-stream-detail'),()=>page.waitForTimeout(220));
   assert.deepEqual(errors,[],name);console.log('PASS '+name);
  }finally{await context.close();}
 }
 await scenario('handling-reading-ui.cjs',async(page,stream,tools,settle)=>{
assert.equal(await page.locator('[data-handling-reading-toggle],[data-handling-stream-status]').count(),0);
 assert(!(await page.locator('#handling-window').innerText()).includes('持续同步进展'));
 for(const size of [{width:1280,height:900},{width:760,height:760},{width:1280,height:540},{width:600,height:480}]){
  await page.setViewportSize(size);await settle();
  const header=await page.locator('.handling-heading').boundingBox(),dock=await page.locator('#handling-dock').boundingBox(),read=await stream.boundingBox(),win=await page.locator('#handling-window').boundingBox();
  assert(read.y>=header.y+header.height-1&&read.y+read.height<=dock.y+1);assert(dock.y+dock.height<=win.y+win.height&&win.y+win.height<=size.height);
  for(const top of [0,500,100000]){await stream.evaluate((el,y)=>el.scrollTop=y,top);await settle();assert(Math.abs((await page.locator('#handling-dock').boundingBox()).y-dock.y)<1);assert(Math.abs((await page.locator('.handling-heading').boundingBox()).y-header.y)<1);}
  assert(await stream.evaluate(el=>el.scrollHeight>el.clientHeight));assert((await stream.textContent()).includes('尾部工具 94'));
  await tools.last().locator('summary').click();await settle();assert(await stream.evaluate(el=>el.scrollHeight-el.clientHeight-el.scrollTop<2));
  await tools.last().locator('summary').click();await settle();assert(await stream.evaluate(el=>el.scrollHeight-el.clientHeight-el.scrollTop<2));
  assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
 }
 const outer=await page.locator('#task-detail').evaluate(el=>el.scrollTop);await stream.hover();await page.mouse.wheel(0,-240);await settle();assert(await page.locator('[data-handling-latest]').isVisible());assert.equal(await page.locator('#task-detail').evaluate(el=>el.scrollTop),outer);
 await page.locator('[data-handling-latest]').click();await settle();assert(await stream.evaluate(el=>el.scrollHeight-el.clientHeight-el.scrollTop<2));
 });
 await scenario('reading-motion-ui.cjs',async(page,stream,tools,settle)=>{
async function frames(index){return page.evaluate(index=>new Promise(resolve=>{
  const output=document.querySelector('[data-handling-stream]'),tool=output.querySelectorAll('details')[index],summary=tool.querySelector('summary'),rows=[],start=performance.now();
  summary.click();
  const frame=()=>{rows.push({gap:output.scrollHeight-output.clientHeight-output.scrollTop,top:output.scrollTop,height:tool.getBoundingClientRect().height,summary:summary.getBoundingClientRect().top});if(performance.now()-start<240)requestAnimationFrame(frame);else resolve(rows);};requestAnimationFrame(frame);
 }),index);}
 for(const reduced of ['no-preference','reduce']){
  await page.emulateMedia({reducedMotion:reduced});await stream.evaluate(el=>{el.scrollTop=el.scrollHeight;});await settle();
  const up=await frames(1);assert(up.every(r=>Math.abs(r.gap)<2),'tail stays attached throughout tool expansion: '+JSON.stringify(up));assert(up.at(-1).top>up[0].top||reduced==='reduce');
  const down=await frames(1);assert(down.every(r=>Math.abs(r.gap)<2),'tail stays attached throughout collapse');
  await stream.evaluate(el=>{const first=el.querySelector('details');el.scrollTop+=first.getBoundingClientRect().top-el.getBoundingClientRect().top-100;});await settle();
  const before=await stream.evaluate(el=>el.scrollTop);const away=await frames(0);assert(away.every(r=>Math.abs(r.top-before)<2),'non-tail tool grows below the current position');
  const close=await frames(0);assert(close.every(r=>Math.abs(r.top-before)<2));
 }
 await page.emulateMedia({reducedMotion:'no-preference'});await stream.evaluate(el=>el.scrollTop=el.scrollHeight);await settle();
 await page.evaluate(()=>{const s=document.querySelectorAll('.handling-stream-detail summary')[1];s.click();setTimeout(()=>s.click(),55);});await page.waitForTimeout(280);assert(!(await tools.last().evaluate(el=>el.open)));assert(await stream.evaluate(el=>el.scrollHeight-el.clientHeight-el.scrollTop<2));
 await page.evaluate(()=>{document.querySelectorAll('.handling-stream-detail summary')[1].click();setTimeout(()=>{fixture.handling[0].rounds[0].steps.at(-1).text+='\n新的完整进展';updateHandlingLive();},50);});await settle();assert(await stream.evaluate(el=>el.scrollHeight-el.clientHeight-el.scrollTop<2));
 await stream.evaluate(el=>el.scrollTop-=200);await settle();await page.evaluate(()=>{fixture.handling[0].rounds[0].steps.at(-1).text+='\n不打断上翻';updateHandlingLive();});assert(await page.locator('[data-handling-latest]').isVisible());
 });
 await scenario('reading-modal-fixes-ui.cjs',async(page,stream,tools,settle)=>{
assert((await page.locator('[data-handle="start"]').innerText()).includes('AI 处置中'));
 await page.locator('#handling-question').fill('保留独立窗口草稿');await tools.last().locator('summary').click();await settle();await stream.evaluate(el=>el.scrollTop=300);await settle();
 await page.evaluate(()=>window.dispatchEvent(new Event('blur')));await page.mouse.click(5,5);assert(await page.locator('#handling-window').isVisible());
 await page.locator('[data-page="schedules"]').first().click();assert(await page.locator('#handling-window').isVisible());assert.equal(await page.locator('#handling-question').inputValue(),'保留独立窗口草稿');
 await page.evaluate(()=>refreshState(false));assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='state').at(-1).params.task),'one');
 await page.locator('[data-page="tasks"]').first().click();await page.evaluate(()=>{selected='two';renderDetail();});assert((await page.locator('#handling-window-title').innerText()).includes('one'));
 await page.locator('[data-handling-close]').click();assert.equal(await page.locator('#handling-window').count(),0);await page.evaluate(()=>{selected='one';renderDetail();});await page.locator('[data-handle="start"]').click();
 assert.equal(await page.locator('#handling-question').inputValue(),'保留独立窗口草稿');assert.equal(await stream.evaluate(el=>el.scrollTop),300);assert(await tools.last().evaluate(el=>el.open));
 assert.equal(await page.locator('#handling-window [data-handle="history"]').count(),0);await page.evaluate(()=>setModal('<h2>合成子窗口</h2><button data-action="close">关闭</button>'));await page.locator('#overlay').waitFor();await page.keyboard.press('Escape');await page.locator('#overlay').waitFor({state:'hidden'});assert(await page.locator('#handling-window').isVisible());
 await page.evaluate(async()=>{fixture.handling[0].model_reported=false;fixture.handling[0].last_reported_model='verified-model';fixture.handling[0].last_reported_effort='high';await refreshState(true);});assert.equal(await page.locator('.handling-composer-model').innerText(),'verified-model（上次报告） · 高');
 await page.evaluate(async()=>{fixture.handling[0].model_reported=true;fixture.handling[0].model='new-model';await refreshState(true);});assert((await page.locator('.handling-composer-model').innerText()).startsWith('new-model'));
 await page.keyboard.press('Escape');assert.equal(await page.locator('#handling-window').count(),0);assert.equal(await page.evaluate(()=>requests.filter(r=>['handling_start','handling_followup','handling_terminate'].includes(r.method)).length),0);
 });
}).catch(e=>{console.error(e);process.exitCode=1});
