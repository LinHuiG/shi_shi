const fs=require('fs'),path=require('path'),assert=require('assert'),{chromium}=require('playwright');
(async()=>{const browser=await chromium.launch({headless:true,executablePath:['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe','C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync),args:['--disable-background-networking']});try{
 const page=await browser.newPage({viewport:{width:1280,height:900}}),errors=[];page.on('pageerror',e=>errors.push(e.message));page.setDefaultTimeout(8000);await page.setContent(require('./handling-window-fixture.cjs')());await page.locator('[data-handle="start"]').click();await page.locator('#handling-window').waitFor();
 const stream=page.locator('[data-handling-stream]'),tools=page.locator('.handling-stream-detail'),settle=()=>page.waitForTimeout(220);

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
 assert.deepEqual(errors,[]);console.log('PASS '+path.basename(__filename));
}finally{await browser.close();}})().catch(e=>{console.error(e);process.exitCode=1});
