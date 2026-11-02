const assert=require('node:assert/strict'),{withBrowser}=require('./ui-driver.cjs');
withBrowser(async(browser)=>{
  const page=await browser.newPage({viewport:{width:1100,height:800},reducedMotion:'reduce'}),errors=[];
  page.on('pageerror',error=>errors.push(error.message));
  await page.setContent(require('./handling-window-fixture.cjs')());
  await page.locator('[data-handle="start"]').click();
  await page.evaluate(async()=>{
   fixture.core_revision='synthetic-core';fixture.handling_revision=1;
   const run=fixture.handling[0];run._details=true;run._base_updated=10;run._stream_revision=[10,1];
   run.rounds=Array.from({length:30},(_,r)=>({id:'r'+r,steps:Array.from({length:100},(_,i)=>({id:'s'+i,kind:'commandExecution',text:'echo synthetic\n'+'x'.repeat(400)}))}));
   window.ipc.postMessage=raw=>{
    const request=JSON.parse(raw);requests.push(request);let data=true;
    if(request.method==='state'){
     data=request.params.core_revision===fixture.core_revision
      ? {_unchanged:true,_page:request.params.page,core_revision:fixture.core_revision,handling_revision:fixture.handling_revision,activity:fixture.activity,activity_day_revision:fixture.activity_day_revision,...window.nextRuntime}
      : {...structuredClone(fixture),_page:request.params.page};
     if(!data._unchanged){
      const detailed=new Set([request.params.task,request.params.selected_task]);
      data.handling=data.handling.map(run=>detailed.has(run.task_id)?{...run,_details:true}:{id:run.id,task_id:run.task_id,status:run.status,transport:run.transport,project_name:run.project_name,_details:false});
     }
     window.nextRuntime=undefined;
    }
    if(request.method==='activity_query')data={day:request.params.day,rows:[],page:1,pages:1,total:0,revision:fixture.activity_day_revision};
    const delay=request.method==='state'?(window.delayState||1):1;window.delayState=0;
    setTimeout(()=>receive({id:request.id,ok:true,data}),delay);
   };
   await refreshState(true);
  });
  const unchanged=await page.evaluate(()=>{
   const output=document.querySelector('[data-handling-stream]'),observer=new MutationObserver(()=>{});
   observer.observe(output,{subtree:true,childList:true,attributes:true,characterData:true});
   const start=performance.now();for(let i=0;i<10;i++)updateHandlingLive();
   const result={ms:(performance.now()-start)/10,mutations:observer.takeRecords().length};observer.disconnect();return result;
  });
  assert.equal(unchanged.mutations,0);
  await page.evaluate(async()=>{
   window.oldRound=state.handling[0].rounds[0];window.oldIcon=document.querySelector('.handling-tool-icon svg');
   const changed=structuredClone(state.handling[0].rounds[29]);changed.steps[99].text='latest synthetic output';
   fixture.handling_revision=2;
   window.nextRuntime={handling_live:[{id:fixture.handling[0].id,_base_updated:10,_stream_revision:[11,2],_details:true,_round_count:30,_round_changes:[[29,changed]]}]};
   await refreshState(false);
  });
  assert(await page.evaluate(()=>state.handling[0].rounds[0]===oldRound&&document.querySelector('.handling-tool-icon svg')===oldIcon));
  assert.equal(await page.locator('[data-stream-entry="r29:step:s99"] .handling-stream-text').textContent(),'latest synthetic output');
  await page.evaluate(async()=>{
   const restored=structuredClone(fixture.handling[0].rounds[29]);fixture.handling_revision=3;
   window.nextRuntime={handling_live:[{id:fixture.handling[0].id,_base_updated:10,_stream_revision:[12,3],_details:true,_round_count:30,_round_changes:[[29,restored]]}]};
   await refreshState(false);
  });
  assert((await page.locator('[data-stream-entry="r29:step:s99"] .handling-stream-text').textContent()).startsWith('echo synthetic'),'a preview reverting to durable text replaces the previous preview');
  // Local callers without a backend revision may mutate an object in place.
  await page.evaluate(()=>{delete state.handling[0]._stream_revision;state.handling[0].rounds[29].steps[99].text='local edit';updateHandlingLive();});
  assert.equal(await page.locator('[data-stream-entry="r29:step:s99"] .handling-stream-text').textContent(),'local edit');
  await page.evaluate(async()=>{
   fixture.handling[0]._base_updated=20;fixture.handling[0]._stream_revision=[20,4];fixture.handling[0].rounds[29].steps[99].text='fresh durable history';
   fixture.handling_revision=4;window.beforeRetry=requests.length;
   window.nextRuntime={handling_live:[{id:fixture.handling[0].id,_base_updated:20,_stream_revision:[20,4],_round_count:30,_round_changes:[]}]};
   await refreshState(false);
  });
  assert(await page.evaluate(()=>requests.slice(beforeRetry).filter(r=>r.method==='state').length===2));
  assert.equal(await page.locator('[data-stream-entry="r29:step:s99"] .handling-stream-text').textContent(),'fresh durable history');
  await page.evaluate(async()=>{window.delayState=50;const pending=refreshState(false);selected='two';await pending;});
  assert(await page.evaluate(()=>selected==='two'&&handlingWindow.id==='one'&&requests.filter(r=>r.method==='state').at(-1).params.selected_task==='two'),'a stale selection response cannot replace the newly selected task or close the open conversation');
  await page.evaluate(async()=>{
   page='logs';logDay='2026-01-02';fixture.usage={calls:0,input_tokens:0,output_tokens:0,cached_reports:0};fixture.activity_day_revision={day:logDay,files:1,audit:'unchanged'};
   await refreshState(true);window.oldQueries=requests.filter(r=>r.method==='activity_query').length;
   fixture.activity=[{time:Date.now(),kind:'scan',detail:{rows:1}}];await refreshState(false);
  });
  assert(await page.evaluate(()=>requests.filter(r=>r.method==='activity_query').length===oldQueries),'today activity must not reload a historical day');
  await page.evaluate(async()=>{
   // Real usage is committed to state, independently of the selected day's log.
   fixture.usage={calls:4,input_tokens:10,output_tokens:5,cached_reports:0};fixture.core_revision='usage-committed';
   await refreshState(false);
  });
  assert(await page.evaluate(()=>requests.filter(r=>r.method==='activity_query').length===oldQueries),'a usage commit must not re-query historical logs');
  assert((await page.locator('.usage-line').innerText()).includes('15'),'today usage stays fresh without re-querying historical logs');
  await page.evaluate(async()=>{fixture.activity_day_revision={day:logDay,files:2,audit:'unchanged'};await refreshState(false);});
  assert(await page.evaluate(()=>requests.filter(r=>r.method==='activity_query').length===oldQueries+1),'selected-day file changes must refresh even on runtime snapshots');
  // Opening notification B must request both B and the independent window A.
  assert(await page.evaluate(()=>openNotifiedTask('two')));
  assert(await page.evaluate(()=>{
   const params=requests.filter(request=>request.method==='state').at(-1).params;
   return page==='tasks'&&selected==='two'&&handlingWindow.id==='one'
    &&params.task==='one'&&params.selected_task==='two'
    &&state.handling.find(run=>run.task_id==='one').rounds.length===30
    &&state.handling.find(run=>run.task_id==='two').rounds.length===1;
  }),'notification navigation retains complete histories for the open conversation and selected task');
  assert.equal(await page.locator('[data-stream-entry="r29:step:s99"] .handling-stream-text').textContent(),'fresh durable history');
  // A command must wait for the real target page before clicking its action.
  const navigation=await page.evaluate(async()=>{
   const clicks=[];
   const onClick=event=>{
    if(event.target.closest('[data-action="new-schedule"]'))clicks.push({page,statePage:state._page,rendered:!!document.querySelector('.schedule-page')});
   };
   document.addEventListener('click',onClick,true);window.delayState=80;
   try{await runAction('schedules','new-schedule');return {clicks,dialog:!$('#overlay').hidden,form:!!$('#s-title')};}
   finally{document.removeEventListener('click',onClick,true);}
  });
  assert.deepEqual(navigation,{clicks:[{page:'schedules',statePage:'schedules',rendered:true}],dialog:true,form:true});
  const indexed = await page.evaluate(() => {
   // Count record visits instead of imposing a hardware-dependent time limit.
   const previous = state, rows = [], count = 5000;
   let visits = 0;
   for (let index = 0; index < count * 2; index++) {
    const run = {id: String(index), task_id: String(index % count), status: index % 7 ? 'completed' : 'setupCancelled', current_source: index % 11 !== 0};
    rows.push(new Proxy(run, {get(target, key) {if(key === 'task_id') visits++; return target[key];}}));
   }
   try {
    state = {...state, handling: rows};
    const started = performance.now(); indexHandlingRuns(true);
    const actual = Array.from({length: count}, (_, index) => taskRun(String(index))?.id);
    const ms = performance.now() - started, indexedVisits = visits;
    const expected = Array.from({length: count}, (_, index) => rows.filter(run => run.task_id === String(index) && run.status !== 'setupCancelled' && run.current_source !== false).at(-1)?.id);
    const equal = JSON.stringify(actual) === JSON.stringify(expected);
    state.handling = [{id:'replacement',task_id:'0',status:'inProgress'}];
    const replaced = taskRun('0')?.id === 'replacement';
    state.handling.push({id:'appended',task_id:'0',status:'completed'});
    const appended = taskRun('0')?.id === 'appended';
    state.handling[1].status = 'setupCancelled';
    const mutated = taskRun('0')?.id === 'replacement';
    return {equal, replaced, appended, mutated, visits:indexedVisits, records:rows.length, ms};
   } finally {state = previous;}
  });
  assert(indexed.equal && indexed.replaced && indexed.appended && indexed.mutated, JSON.stringify(indexed));
  assert.equal(indexed.visits,indexed.records,'one index traversal for all 5,000 task lookups');
  console.log('Indexed handling lookup:',JSON.stringify(indexed));
  assert.deepEqual(errors,[]);
  console.log('PASS scoped runtime: unchanged 3,000-step DOM '+unchanged.ms.toFixed(3)+' ms, no mutations; round delta merge; stale base reload; local edits; historical-day invalidation; notification preserves both histories; cross-page command waits for rendered actions.');
 }).catch(error=>{console.error(error);process.exitCode=1;});
