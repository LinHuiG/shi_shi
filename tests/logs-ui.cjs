const fs=require('fs'),path=require('path'),assert=require('assert'),{withBrowser}=require('./ui-driver.cjs');
const out=fs.mkdtempSync(path.join(require('os').tmpdir(),'shishi-logs-ui-'));
const fixture={config:{codex:'fixture.exe',codex_models:[],leaders:[],enabled:false,auto_ai:false,interval_minutes:5,analysis_interval_minutes:30,analysis_lookback_hours:24,daily_calls:40,max_input_chars:60000,message_retention_days:90,work_start:'09:00',work_end:'18:00'},tasks:[],schedules:[],messages:[],audit:[],activity:[],reports:[],handling:[],chat_sources:[],suggestions:[],usage:{calls:0,input_tokens:0,output_tokens:0,cached_reports:0,unreported_calls:0}};
const mock=`window.fixture=${JSON.stringify(fixture)};window.requests=[];window.logRows=Array.from({length:55},(_,i)=>({kind:'ai_success',time:Date.now()-100000+i,detail:{model:i?'later-fixture':'early-fixture',request_id:'fixture-'+i,elapsed_ms:1000}})).concat(Array.from({length:80},(_,i)=>({kind:'scan',time:Date.now()+i,detail:{rows:1,elapsed_ms:10}})),[{kind:'error',time:Date.now(),detail:{error:'synthetic failure'}}]);window.ipc={postMessage(raw){const {id,method,params:p}=JSON.parse(raw);requests.push({method,p});setTimeout(()=>{let data=true;if(method==='state')data=fixture;if(method==='activity_query'){if(window.failLogs){window.receive({id,ok:false,error:'synthetic read failure'});return;}const rows=p.day==='2026-10-18'?[{kind:'ai_success',time:Date.now(),detail:{model:'stale-day'}}]:logRows.filter(r=>p.filter==='all'||(p.filter==='ai'?r.kind==='ai_success':p.filter==='errors'?r.kind==='error':false)).sort((a,b)=>b.time-a.time);const pages=Math.max(1,Math.ceil(rows.length/p.page_size)),page=Math.min(p.page,pages);data={rows:rows.slice((page-1)*p.page_size,page*p.page_size),total:rows.length,page,pages};}window.receive({id,ok:true,data});},method==='activity_query'&&p.day==='2026-10-18'?200:5);}};`;
withBrowser(async(browser)=>{
const page=await browser.newPage({viewport:{width:1280,height:900}}),errors=[];page.on('pageerror',e=>errors.push(e.message));await page.setContent(require('../scripts/frontend.cjs').build().replace('<script>','<script>'+mock));await page.waitForFunction(()=>!!state?.config);await page.locator('[data-page="logs"]').click();await page.getByText('共 55 条 · 第 1 / 2 页').waitFor();assert.equal(await page.locator('.usage-note').count(),0);assert.equal(await page.locator('.timeline .event').count(),50);assert(!(await page.locator('#view').innerText()).includes('early-fixture'));
await page.locator('[data-log-page="2"]').click();await page.getByText('共 55 条 · 第 2 / 2 页').waitFor();assert((await page.locator('#view').innerText()).includes('early-fixture'),'morning analysis remains visible after scanning noise');
await page.locator('[data-log-filter="errors"]').click();await page.getByText('共 1 条 · 第 1 / 1 页').waitFor();assert((await page.locator('#view').innerText()).includes('synthetic failure'));await page.locator('[data-log-filter="ai"]').click();await page.getByText('共 55 条 · 第 1 / 2 页').waitFor();
const current=await page.locator('#log-day').inputValue();await page.locator('#log-day').fill('2026-10-18');await page.locator('#log-day').press('Tab');await page.waitForFunction(()=>requests.some(r=>r.method==='activity_query'&&r.p.day==='2026-10-18'));await page.locator('#log-day').fill(current);await page.locator('#log-day').press('Tab');await page.waitForTimeout(250);assert(!(await page.locator('#view').innerText()).includes('stale-day'));
await page.evaluate(()=>window.failLogs=true);await page.locator('[data-log-filter="all"]').click();await page.getByText('日志加载失败',{exact:true}).waitFor();await page.evaluate(()=>window.failLogs=false);await page.locator('[data-action="retry-logs"]').click();await page.getByText('共 136 条 · 第 1 / 3 页').waitFor();await page.setViewportSize({width:760,height:900});assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));await page.screenshot({path:path.join(out,'logs-pagination.png'),fullPage:true});await page.evaluate(async()=>{
 const base={selected:23,pending:80,message_chars:4120,total_input_chars:58420,max_input_chars:60000,initial_selected:40,reduced_by:['reply_context','task_summaries','input_budget'],message_budget_chars:99999},
  row=(id,kind,batch,extra={})=>({id,kind,time:Date.now(),detail:{model:'synthetic-budget',elapsed_ms:1000,batch,...extra}});
 logRows=[
  row('reduced','ai_success',base),
  row('unreduced','ai_success',{selected:4,pending:4,message_chars:1200,total_input_chars:22400,max_input_chars:80000,initial_selected:4,reduced_by:[]}),
  row('legacy','ai_success',{selected:10,pending:70,message_chars:3200,message_budget_chars:90000,batch_limit_chars:70000}),
  row('legacy-start','ai_start',{selected:3,pending:12,message_chars:2000,batch_limit_chars:50000},{messages:3,input_sizes:{total_input_chars:12345,max_input_chars:30000}}),
  row('failed','ai_failed',{...base,reduced_by:['input_budget']},{error:'synthetic failed: <not-markup>\nkeep this detail'}),
  row('filter','ai_success',{...base,reduced_by:['task_summaries'],selected:40,initial_selected:40},{kind:'message_filter'}),
  row('legacy-limit','ai_cancelled',{selected:2,pending:9,message_chars:800,batch_limit_chars:45000}),
  row('malformed','ai_success',{selected:1,pending:1,message_chars:0,total_input_chars:'<img src=x>',max_input_chars:null,reduced_by:['unknown-reason']})
 ];
 logFilter='all';logPage=1;await renderLogs();
});
const batchText=id=>page.locator(`[data-event-key="${id}"] .event-description`).first().innerText();
const reduced=await batchText('reduced');
assert(reduced.includes('请求占用 58,420/60,000 字符 · 本批 23/80 条 · 消息 4,120 字符'));
assert(reduced.includes('缩批 40→23 条（输入上限）'));
assert.equal(await page.locator('[data-event-key="reduced"] .event-description span[title]').getAttribute('title'),'精简引用摘录；精简事项摘要；按完整请求输入上限缩批');
assert(!reduced.includes('99,999')&&!reduced.includes('消息体积'));
const unreduced=await batchText('unreduced');
assert(unreduced.includes('请求占用 22,400/80,000 字符'));
assert(!unreduced.includes('缩批')&&!unreduced.includes('精简'));
const legacy=await batchText('legacy');
assert(legacy.includes('本批 10/70 条 · 消息 3,200 字符'));
assert(!legacy.includes('请求占用')&&!legacy.includes('90,000')&&!legacy.includes('70,000'));
const start=await batchText('legacy-start');
assert(start.includes('请求占用 12,345/30,000 字符')&&start.includes('本批 3/12 条'));
assert.equal((start.match(/本批/g)||[]).length,1,'batch counts are not repeated in start records');
assert((await batchText('failed')).includes('synthetic failed: <not-markup>\nkeep this detail'));
assert((await batchText('failed')).includes('请求占用 58,420/60,000 字符'));
assert((await batchText('filter')).includes('精简事项摘要'));
assert.equal(await page.locator('[data-event-key="filter"] .event-heading b').innerText(),'消息过滤完成');
assert((await batchText('legacy-limit')).includes('本批 2/9 条 · 消息 800 字符'));
assert(!(await batchText('legacy-limit')).includes('45,000'));
assert((await batchText('malformed')).includes('消息 0 字符'));
assert(!(await page.locator('.timeline').innerText()).match(/undefined|NaN|unknown-reason/));
assert.equal(await page.locator('.timeline img,.timeline not-markup').count(),0);
assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),'batch details fit the narrow log page');
await page.screenshot({path:path.join(out,'batch-request-budget.png'),fullPage:true});
assert.deepEqual(errors,[]);console.log('PASS disk-query UI contract; early AI beyond scanning ring; date/filter/page; stale response guard; read failure/retry; complete request budgets, compact reduction causes, legacy records, unchanged error text, filter batches and narrow layout. '+out);
}).catch(e=>{console.error(e);process.exitCode=1});
