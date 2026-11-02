// Real UI updates with synthetic IPC: bounded latency, stable controls and Unicode.
const fs=require('fs'),path=require('path'),assert=require('assert'),{withBrowser}=require('./ui-driver.cjs');
const out=fs.mkdtempSync(path.join(require('os').tmpdir(),'shishi-motion-')),now=Date.now();
const fixture={config:{enabled:false,auto_ai:false,interval_minutes:5,leaders:[],daily_calls:40,work_start:'09:00',work_end:'18:00'},usage:{day:'fixture',calls:0,input_tokens:0,output_tokens:0},tasks:[{id:'task',title:'合成事项',description:'保持阅读位置。',status:'待处理',today:true,created:now,updated:now}],handling:[{id:'run',task_id:'task',thread_id:'synthetic',status:'inProgress',project_name:'合成项目',rounds:[{id:'round',turn_id:'turn',status:'inProgress',steps:[]}],requests:[]}],schedules:[],messages:[],audit:[],activity:[],reports:[],suggestions:[],chat_sources:[],pending:0,ai_busy:false};
const mock=`window.fixture=${JSON.stringify(fixture)};window.requests=[];window.failSend=false;window.ipc={postMessage(raw){const r=JSON.parse(raw);requests.push(r);setTimeout(()=>{let data=true,error='';if(r.method==='state')data=fixture;if(r.method==='evidence')data=[];if(r.method==='handling_followup'&&failSend)error='合成发送失败';if(r.method==='activity_query')data={rows:[{kind:'ai_failed',time:${now},detail:{kind:'classify',response_received:true,error:'合成编号校验失败',request_id:'fixture'}}],total:1,pages:1,page:1};receive({id:r.id,ok:!error,data,error});},2)}};`;
withBrowser(async(browser)=>{
 const page=await browser.newPage({viewport:{width:1280,height:900}}),errors=[];page.on('pageerror',e=>errors.push(e.message));await page.setContent(require('./ui-bundle.cjs')().replace('<script>','<script>'+mock));await page.locator('[data-handle="start"]').click();await page.locator('[data-handling-stream]').waitFor();
 await page.locator('#handling-question').click();await page.locator('#handling-question').fill('保留我的草稿');
 await page.evaluate(()=>{window.streamNode=document.querySelector('[data-handling-stream]');window.rowNode=document.querySelector('[data-task="task"]');window.fieldNode=document.querySelector('#handling-question');fieldNode.setSelectionRange(2,2);});
 const text=Array.from({length:70},(_,i)=>`第 ${i} 行：中文路径 D:/合成资料/文件.txt 与 emoji 🚀🙂。`).join('\n');
 await page.evaluate(async text=>{fixture.handling[0].rounds[0].steps=[{id:'answer',kind:'agentMessage',text}];fixture.handling[0].progress=text;await refreshState(false);},text);
 await page.waitForTimeout(220);
 assert.equal(await page.locator('.handling-stream-text').innerText(),text);assert(await page.locator('[data-handling-stream]').evaluate(el=>el.clientHeight>360));
 assert(await page.evaluate(()=>streamNode===document.querySelector('[data-handling-stream]')&&rowNode===document.querySelector('[data-task="task"]')&&fieldNode===document.querySelector('#handling-question')&&document.activeElement===fieldNode&&fieldNode.selectionStart===2));
 assert.equal(await page.locator('#handling-question').inputValue(),'保留我的草稿');
 await page.locator('[data-handling-stream]').evaluate(el=>el.scrollTop=500);await page.waitForTimeout(60);
 await page.evaluate(async()=>{fixture.handling[0].rounds[0].steps[0].text+='\n新增内容 🚀';await refreshState(false);});await page.waitForTimeout(220);
 assert.equal(await page.locator('[data-handling-stream]').evaluate(el=>el.scrollTop),500);assert(await page.locator('[data-handling-latest]').isVisible());
 await page.locator('[data-handling-latest]').click();await page.waitForTimeout(220);assert(await page.locator('[data-handling-stream]').evaluate(el=>el.scrollHeight-el.clientHeight-el.scrollTop<2));
 await page.evaluate(async()=>{const row=fixture.handling[0].rounds[0].steps[0];window.fullProgress=row.text;row.text=row.text.slice(0,40);await refreshState(false);});await page.waitForTimeout(180);assert.equal(await page.locator('.handling-stream-text').innerText(),await page.evaluate(()=>fullProgress));
 await page.locator('#handling-question').focus();await page.locator('#handling-question').press('Control+Enter');assert.equal(await page.locator('#handling-question').inputValue(),'保留\n我的草稿');
 await page.evaluate(()=>document.querySelector('#handling-question').dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',isComposing:true,bubbles:true})));assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='handling_followup').length),0);
 await page.evaluate(()=>failSend=true);await page.locator('#handling-question').press('Enter');await page.waitForFunction(()=>document.querySelector('#handling-inline-error')?.textContent.includes('合成发送失败'));assert((await page.locator('#handling-question').inputValue()).includes('草稿'));
 await page.evaluate(()=>failSend=false);await page.locator('#handling-question').press('Enter');await page.waitForFunction(()=>document.querySelector('#handling-question')?.value==='');assert.equal(await page.evaluate(()=>requests.filter(r=>r.method==='handling_followup').length),2);
 async function approval(count){await page.evaluate(async count=>{fixture.handling[0].status='waiting';fixture.handling[0].requests=[{id:'permission',method:'dsh/tool/requestApproval',params:{tool_call:{title:'读取合成文件',rawInput:{paths:Array.from({length:count},(_,i)=>'D:/fixture/'+i)}},dsh_options:[{id:'once',name:'允许',kind:'allow_once'},{id:'no',name:'拒绝',kind:'reject_once'}]}}];await refreshState(true);handlingRequests('task');},count);await page.waitForTimeout(180);return page.locator('#dialog').boundingBox();}
 const short=await approval(1),long=await approval(100);assert(Math.abs(short.height-long.height)<1&&Math.abs(short.width-long.width)<1);assert(long.width<=560);
 assert(await page.locator('.approval-scroll').evaluate(el=>el.scrollHeight>el.clientHeight&&getComputedStyle(el).overflowY==='auto'));
 const footer=await page.locator('.approval-footer').boundingBox();await page.locator('.approval-scroll').evaluate(el=>el.scrollTop=el.scrollHeight);assert(Math.abs(footer.y-(await page.locator('.approval-footer').boundingBox()).y)<1);
 const mainScroll=await page.locator('#task-detail').evaluate(el=>el.scrollTop);await page.locator('.approval-scroll').hover();await page.mouse.wheel(0,1000);await page.waitForTimeout(180);assert.equal(await page.locator('#task-detail').evaluate(el=>el.scrollTop),mainScroll);
 await page.screenshot({path:path.join(out,'permission.png')});await page.locator('[data-action="close"]').click();
 await page.evaluate(async()=>{fixture.ai_busy=true;fixture.ai_live={request_id:'live',phase:'合成分析',entries:[{id:'entry',kind:'content',text:'短文字'}]};await refreshState(false);});await page.locator('[data-ai-live]').click();await page.waitForTimeout(180);const liveShort=await page.locator('#dialog').boundingBox();
 await page.evaluate(async text=>{fixture.ai_live.entries[0].text=text;await refreshState(false);},text);await page.waitForTimeout(220);const liveLong=await page.locator('#dialog').boundingBox();assert(Math.abs(liveShort.height-liveLong.height)<1);assert.equal(await page.locator('.live-entry-text').innerText(),text);
 await page.evaluate(async()=>{fixture.ai_live.entries[0].text='A🚀'+'x'.repeat(259);await refreshState(false);});assert((await page.locator('[data-live-line]').innerText()).startsWith('🚀'));
 assert(await page.evaluate(()=>document.getAnimations().every(a=>a.effect.getTiming().duration<=160||a.effect.getTiming().iterations===Infinity)));
 await page.emulateMedia({reducedMotion:'reduce'});await page.evaluate(()=>motionText(document.querySelector('#ai-live-detail-status'),'减少动态效果'));assert.equal(await page.locator('#ai-live-detail-status').innerText(),'减少动态效果');
 await page.locator('[data-action="close"]').click();await page.evaluate(async()=>{page='logs';await render();});await page.locator('.timeline').waitFor();assert((await page.locator('.timeline').innerText()).includes('AI 已返回 · 结果处理失败'));
 // Same-input updates still repair external DOM edits, including changes whose
 // observer callback has not run; live form properties keep the user's draft.
 const cacheCases=await page.evaluate(async()=>{
  const host=document.createElement('section');document.body.append(host);
  const html='<div><span>原文</span><!--注释--><input value="初值"><details><summary>详情</summary><p>正文</p></details></div>';
  const results=[];
  for(const deferred of [false,true])for(const mutation of ['text','attribute','child','nested','comment']){
   setHtml(host,html);setHtml(host,html);
   if(mutation==='text')host.querySelector('span').firstChild.data='外部改动';
   if(mutation==='attribute')host.querySelector('span').title='外部改动';
   if(mutation==='child')host.querySelector('span').append(document.createElement('b'));
   if(mutation==='nested')setHtml(host.firstChild,'<p>嵌套更新</p>');
   if(mutation==='comment')host.firstChild.childNodes[1].data='外部注释';
   if(deferred)await Promise.resolve();
   setHtml(host,html);results.push([deferred,mutation,host.innerHTML===html]);
  }
  const input=host.querySelector('input'),details=host.querySelector('details');
  input.value='中文草稿 🚀';input.focus();input.setSelectionRange(2,4,'backward');details.open=true;
  setHtml(host,html.replace('原文','新文'));
  results.push(['draft',input===host.querySelector('input'),input.value,document.activeElement===input,input.selectionStart,input.selectionEnd,input.selectionDirection,details.open]);
  const items='<span>一</span><button>二</button><span>三</span><i data-ui-key="same">旧一</i><i data-ui-key="same">旧二</i>';
  setHtml(host,items);const old=[...host.children];
  setHtml(host,'<button>新二</button><span>新一</span><span>新三</span><i data-ui-key="same">新甲</i><i data-ui-key="same">新乙</i>');
  results.push(['identity',...Array.from(host.children,n=>old.indexOf(n))]);
  setHtml(host,'<select><option value="old">旧选项</option></select><textarea></textarea>');
  setHtml(host,'<select><option value="new">新选项</option><option selected value="selected">选中项</option></select><textarea>新正文</textarea>');
  results.push(['form',host.querySelector('select').value,host.querySelector('textarea').value]);host.remove();return results;
 });
 assert(cacheCases.slice(0,10).every(row=>row[2]),JSON.stringify(cacheCases));
 assert.deepEqual(cacheCases[10],['draft',true,'中文草稿 🚀',true,2,4,'backward',true]);
 assert.deepEqual(cacheCases[11],['identity',1,0,2,4,-1]);
 assert.deepEqual(cacheCases[12],['form','selected','新正文']);
 assert.deepEqual(errors,[]);console.log('PASS bounded motion, stable live interaction and DOM cache invalidation. '+out);
}).catch(e=>{console.error(e);process.exitCode=1});
