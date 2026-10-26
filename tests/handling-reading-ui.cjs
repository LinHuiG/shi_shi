const fs=require('fs'),path=require('path'),assert=require('assert'),{chromium}=require('playwright');
(async()=>{const browser=await chromium.launch({headless:true,executablePath:['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe','C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync),args:['--disable-background-networking']});try{
 const page=await browser.newPage({viewport:{width:1280,height:900}}),errors=[];page.on('pageerror',e=>errors.push(e.message));page.setDefaultTimeout(8000);await page.setContent(require('./handling-window-fixture.cjs')());await page.locator('[data-handle="start"]').click();await page.locator('#handling-window').waitFor();
 const stream=page.locator('[data-handling-stream]'),tools=page.locator('.handling-stream-detail'),settle=()=>page.waitForTimeout(220);

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
 assert.deepEqual(errors,[]);console.log('PASS '+path.basename(__filename));
}finally{await browser.close();}})().catch(e=>{console.error(e);process.exitCode=1});
