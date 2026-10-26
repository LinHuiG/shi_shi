const taskRecordViews = new Map();
let taskRecordPanel=null, taskRecordEpoch=0, taskRecordTimer=0, taskRecordPointer=null;
function recordView(id,kind) {
  const key=id+':'+kind;
  if(!taskRecordViews.has(key)) taskRecordViews.set(key,{query:'',sort:'desc',start:'',end:'',page:1,top:0,opened:new Set()});
  return taskRecordViews.get(key);
}
function taskRecordButtons(t) {
  return `<div class="task-record-buttons">${[['evidence','消息依据',evidenceCount(t),'≡'],['changes','事项变更记录',t.change_count,'◷']].map(([kind,label,count,icon])=>`<button data-task-record="${kind}" aria-haspopup="dialog" aria-expanded="${taskRecordPanel?.id===t.id && taskRecordPanel.kind===kind}"><span aria-hidden="true">${icon}</span>${label}${Number.isFinite(count)?`<small data-record-count="${kind}">${count}</small>`:''}</button>`).join('')}</div>`;
}
function rememberTaskRecord() {
  const p=taskRecordPanel;if(!p?.loaded)return;
  const v=recordView(p.id,p.kind),list=p.el.querySelector('.task-record-content');
  v.top=list.scrollTop;
  v.opened=new Set([...list.querySelectorAll('details[open]')].map(d=>d.closest('[data-ui-key]')?.dataset.uiKey).filter(Boolean));
}
function closeTaskRecord(focus=false) {
  const p=taskRecordPanel;if(!p)return;
  rememberTaskRecord();++taskRecordEpoch;clearTimeout(taskRecordTimer);
  p.el.remove();taskRecordPanel=null;
  document.querySelectorAll('[data-task-record]').forEach(b=>b.setAttribute('aria-expanded','false'));
  if(focus && p.trigger?.isConnected)p.trigger.focus({preventScroll:true});
}
function positionTaskRecord() {
  const el=taskRecordPanel?.el;if(!el)return;
  const rect=$('#task-detail')?.getBoundingClientRect(),width=Math.min(800,innerWidth-24),height=Math.min(650,innerHeight-96);
  Object.assign(el.style,{width:width+'px',height:Math.max(220,height)+'px',left:Math.max(12,Math.min(innerWidth-width-12,rect?rect.left+(rect.width-width)/2:(innerWidth-width)/2))+'px',top:Math.max(12,(innerHeight-height)/2)+'px'});
}
function syncTaskRecordPanel() {
  const p=taskRecordPanel;if(!p)return;
  if(page!=='tasks'||selected!==p.id||!state.tasks.some(t=>t.id===p.id)){closeTaskRecord();return;}
  positionTaskRecord();
  document.querySelectorAll('[data-task-record]').forEach(b=>b.setAttribute('aria-expanded',String(b.dataset.taskRecord===p.kind)));
  const revision=JSON.stringify([state.task_history_revision,state.messages_revision]);
  if(p.revision!==revision){p.revision=revision;void loadTaskRecord();}
}
function openTaskRecord(kind,trigger) {
  if(!['evidence','changes'].includes(kind))return;
  if(taskRecordPanel?.id===selected&&taskRecordPanel.kind===kind){closeTaskRecord(true);return;}
  closeTaskRecord();
  const id=selected,v=recordView(id,kind),title=kind==='evidence'?'消息依据':'事项变更记录',el=document.createElement('section');
  el.id='task-record-window';el.className='task-record-window';el.setAttribute('role','dialog');el.setAttribute('aria-modal','false');el.setAttribute('aria-labelledby','task-record-title');
  el.innerHTML=`<header class="task-record-header"><div><h3 id="task-record-title">${title}</h3><span class="tiny">${esc(state.tasks.find(t=>t.id===id)?.title||'')}</span></div><button class="quiet" data-record-close aria-label="关闭${title}">×</button></header><div class="task-record-tools"><input id="task-record-search" type="search" maxlength="200" aria-label="搜索${title}" placeholder="${kind==='evidence'?'搜索消息、发送人或来源':'搜索变更内容'}" value="${esc(v.query)}"><button data-record-sort aria-label="切换时间顺序">${v.sort==='asc'?'时间 ↑ 正序':'时间 ↓ 倒序'}</button><details class="record-range"><summary>时间区间<span data-record-range-active>${v.start||v.end?' · 已选':''}</span></summary><div><label>开始（北京时间）<input id="record-start" type="datetime-local" value="${esc(v.start)}"></label><label>结束（含所选分钟）<input id="record-end" type="datetime-local" value="${esc(v.end)}"></label><p id="record-range-error" class="error-box" role="alert"></p><div class="row"><button data-record-range-clear>清除</button><button data-record-range-apply>应用</button></div></div></details></div><div class="task-record-content" tabindex="0" aria-label="${title}内容"><p class="helper">正在读取…</p></div><footer class="task-record-footer"><span role="status" data-record-summary></span><div data-record-pages></div></footer>`;
  document.body.append(el);taskRecordPanel={id,kind,el,trigger,loaded:false,revision:JSON.stringify([state.task_history_revision,state.messages_revision])};
  positionTaskRecord();motionAnimate(el,[{opacity:0,transform:'scale(.985)'},{opacity:1,transform:'scale(1)'}]);
  syncTaskRecordPanel();void loadTaskRecord();el.querySelector('input').focus({preventScroll:true});
}
function recordRange(v) {
  const read=(value,end)=>{if(!value)return null;const time=Date.parse(value+':00+08:00');if(!Number.isFinite(time)||time<0)throw Error('请选择有效的时间区间');return time+(end?59999:0);};
  const since=read(v.start,false),until=read(v.end,true);
  if(since!=null&&until!=null&&since>until)throw Error('开始时间不能晚于结束时间');
  return {since,until};
}
function recordMessage(m) {
  return `<article class="record-message" data-ui-key="message-${esc(m.id)}"><div class="record-avatar" aria-hidden="true">${esc([...(m.sender||'消')][0])}</div><div class="record-message-main"><div class="record-message-heading"><b>${esc(m.sender||'未知发送人')}</b><span>${esc(m.source_name||'历史来源')}${m.group?' · '+esc(m.group):''}</span><time>${esc(date(m.time))}</time></div>${messageBody(m.text||'['+m.kind+']')}${messageImages(m)}</div></article>`;
}
function recordChange(a) {
  return `<article class="event" data-ui-key="change-${esc(a.index)}"><div class="event-dot"></div><div class="event-content"><div class="event-heading"><b>${esc(actionNames[a.kind]||a.kind_label||'事项变更')}</b><time>${esc(date(a.time))}</time></div><div class="event-description">${esc(a.change_summary||a.detail||'')}</div>${changeRows(a)}</div></article>`;
}
async function loadTaskRecord(reset=false) {
  const p=taskRecordPanel;if(!p)return;
  clearTimeout(taskRecordTimer);
  const v=recordView(p.id,p.kind),request=++taskRecordEpoch,list=p.el.querySelector('.task-record-content');
  if(reset){v.top=0;v.opened.clear();p.loaded=false;}else rememberTaskRecord();
  list.setAttribute('aria-busy','true');
  try {
    const data=await rpc(p.kind==='evidence'?'evidence_query':'task_changes',{id:p.id,query:v.query,sort:v.sort,page:v.page,...recordRange(v)});
    if(request!==taskRecordEpoch||taskRecordPanel!==p||selected!==p.id||page!=='tasks')return;
    // The reader may scroll or open details while this request is in flight.
    rememberTaskRecord();
    v.page=data.page||1;const rows=Array.isArray(data.rows)?data.rows:[],retries=p.kind==='evidence'?rows.filter(m=>state.analysis_retries?.[m.id]):[];
    // Explicit filtering starts a new reading position. Reusing old row
    // movement animations here can carry headers above the fixed toolbar.
    if(!p.loaded)list.replaceChildren();
    setHtml(list,`${retries.length?`<p class="error-box">${retries.length} 条依据待重试 · ${esc(state.analysis_retries[retries[0].id])}</p>`:''}${rows.length?(p.kind==='changes'?`<div class="timeline">${rows.map(recordChange).join('')}</div>`:rows.map(recordMessage).join('')):`<p class="record-empty helper">${data.all?'没有符合筛选条件的记录。':'暂无保留的记录。'}</p>`}`);
    for(const d of list.querySelectorAll('details'))d.open=v.opened.has(d.closest('[data-ui-key]')?.dataset.uiKey);
    list.scrollTop=v.top;
    p.loaded=true;
    p.el.querySelector('[data-record-summary]').textContent=`${data.total||0} / ${data.all||0} 条 · 第 ${v.page} / ${data.pages||1} 页`;
    setHtml(p.el.querySelector('[data-record-pages]'),`<button data-record-page="${v.page-1}" ${v.page<=1?'disabled':''}>上一页</button><button data-record-page="${v.page+1}" ${v.page>=(data.pages||1)?'disabled':''}>下一页</button>`);
    const count=document.querySelector(`[data-record-count="${p.kind}"]`);if(count)count.textContent=data.all||0;
  } catch(error) {
    if(request!==taskRecordEpoch||taskRecordPanel!==p)return;
    rememberTaskRecord();p.loaded=false;
    p.el.querySelector('[data-record-summary]').textContent='读取失败';setHtml(list,`<p class="error-box">${esc(error.message)}</p><button data-record-retry>重试</button>`);
  } finally {if(request===taskRecordEpoch)list.removeAttribute('aria-busy');}
}
document.addEventListener('click',e=>{
  const b=e.target.closest('button');if(!b)return;
  if(b.dataset.taskRecord){openTaskRecord(b.dataset.taskRecord,b);return;}
  const p=taskRecordPanel;if(!p||!p.el.contains(b))return;const v=recordView(p.id,p.kind);
  if(b.hasAttribute('data-record-close')){closeTaskRecord(true);return;}
  if(b.hasAttribute('data-record-sort')){v.sort=v.sort==='asc'?'desc':'asc';v.page=1;b.textContent=v.sort==='asc'?'时间 ↑ 正序':'时间 ↓ 倒序';void loadTaskRecord(true);}
  if(b.dataset.recordPage){v.page=Number(b.dataset.recordPage);void loadTaskRecord(true);}
  if(b.hasAttribute('data-record-retry'))void loadTaskRecord();
  if(b.hasAttribute('data-record-range-apply')||b.hasAttribute('data-record-range-clear')){
    const clear=b.hasAttribute('data-record-range-clear'),start=$('#record-start'),end=$('#record-end'),draft={start:clear?'':start.value,end:clear?'':end.value};
    try{recordRange(draft);}catch(error){$('#record-range-error').textContent=error.message;return;}
    Object.assign(v,draft,{page:1});start.value=v.start;end.value=v.end;$('#record-range-error').textContent='';p.el.querySelector('.record-range').open=false;
    p.el.querySelector('[data-record-range-active]').textContent=v.start||v.end?' · 已选':'';void loadTaskRecord(true);
  }
});
function searchTaskRecord(e) {
  if(e.target.id!=='task-record-search'||e.isComposing||!taskRecordPanel)return;
  const p=taskRecordPanel,v=recordView(p.id,p.kind);v.query=e.target.value;v.page=1;
  v.top=0;v.opened.clear();p.loaded=false;
  ++taskRecordEpoch;clearTimeout(taskRecordTimer);taskRecordTimer=setTimeout(()=>{if(taskRecordPanel===p)void loadTaskRecord(true);},180);
}
document.addEventListener('input',searchTaskRecord);document.addEventListener('compositionend',searchTaskRecord);
document.addEventListener('pointerdown',e=>{taskRecordPointer={x:e.clientX,y:e.clientY,outside:!!taskRecordPanel&&!taskRecordPanel.el.contains(e.target),id:e.pointerId};},true);
document.addEventListener('pointerup',e=>{const down=taskRecordPointer,p=taskRecordPanel;taskRecordPointer=null;if(p&&down?.outside&&down.id===e.pointerId&&!p.el.contains(e.target)&&!e.target.closest('[data-task-record]')&&$('#overlay').hidden&&Math.hypot(down.x-e.clientX,down.y-e.clientY)<5)closeTaskRecord();},true);
document.addEventListener('keydown',e=>{if(e.key==='Escape'&&taskRecordPanel&&$('#overlay').hidden){e.preventDefault();closeTaskRecord(true);}});
window.addEventListener('resize',positionTaskRecord);
