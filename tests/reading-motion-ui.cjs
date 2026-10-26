const fs=require('fs'),path=require('path'),assert=require('assert'),{chromium}=require('playwright');
(async()=>{const browser=await chromium.launch({headless:true,executablePath:['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe','C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync),args:['--disable-background-networking']});try{
 const page=await browser.newPage({viewport:{width:1280,height:900}}),errors=[];page.on('pageerror',e=>errors.push(e.message));page.setDefaultTimeout(8000);await page.setContent(require('./handling-window-fixture.cjs')());await page.locator('[data-handle="start"]').click();await page.locator('#handling-window').waitFor();
 const stream=page.locator('[data-handling-stream]'),tools=page.locator('.handling-stream-detail'),settle=()=>page.waitForTimeout(220);

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
 assert.deepEqual(errors,[]);console.log('PASS '+path.basename(__filename));
}finally{await browser.close();}})().catch(e=>{console.error(e);process.exitCode=1});
