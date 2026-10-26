const handlingStreams = new WeakMap();
function handlingLivePanel(r) {
  return `<div class="handling-stream"><div class="handling-stream-output" data-handling-stream="${esc(r.id)}" data-preserve-content tabindex="0" aria-label="AI 处置对话"></div><button class="quiet handling-latest" data-handling-latest hidden>回到最新 ↓</button></div>`;
}
function handlingStreamEnd(output) {
  return Math.max(0,output.scrollHeight-output.clientHeight);
}
function handlingStreamAtLatest(output) { return handlingStreamEnd(output)-output.scrollTop<24; }
function handlingStreamLatest(output,view) {
  output.parentElement.querySelector('[data-handling-latest]').hidden=view.follow||handlingStreamAtLatest(output);
}
function pauseHandlingStream(output,view) {
  view.follow=false;cancelAnimationFrame(view.frame);view.frame=0;view.expected=-1;
  for(const motion of handlingToolMotions.values())if(motion.output===output){motion.follow=false;motion.top=output.scrollTop;}
  handlingStreamLatest(output,view);
}
function followHandlingStream(output,view,immediate=false) {
  if(!view.follow)return;
  if(immediate){cancelAnimationFrame(view.frame);view.frame=0;output.scrollTop=handlingStreamEnd(output);view.expected=output.scrollTop;handlingStreamLatest(output,view);return;}
  if(view.frame||[...handlingToolMotions.values()].some(m=>m.output===output))return;
  const start=performance.now(),top=output.scrollTop;
  const step=now=>{
    view.frame=0;if(!output.isConnected||!view.follow)return;
    const end=handlingStreamEnd(output),t=motionReduced()?1:Math.min(1,(now-start)/motionDuration);
    output.scrollTop=top+(end-top)*(1-(1-t)**3);view.expected=output.scrollTop;handlingStreamLatest(output,view);
    if(t<1&&Math.abs(end-output.scrollTop)>1)view.frame=requestAnimationFrame(step);
  };view.frame=requestAnimationFrame(step);
}
const handlingStreamSizes=new ResizeObserver(entries=>{
  for(const {target:output} of entries){const view=handlingStreams.get(output);if(view&&output.isConnected){if(view.follow)followHandlingStream(output,view,true);else handlingStreamLatest(output,view);}}
});
function resizeHandlingStreams(){for(const output of document.querySelectorAll('[data-handling-stream]')){const view=handlingStreams.get(output);if(view){if(view.follow)followHandlingStream(output,view,true);else handlingStreamLatest(output,view);}}}
function updateHandlingLive() {
  for(const output of document.querySelectorAll('[data-handling-stream]')) {
    const run=(state?.handling||[]).find(r=>r.id===output.dataset.handlingStream);if(!run)continue;
    let view=handlingStreams.get(output);
    const initial=!view,saved=handlingConversationViews.get(run.id);
    if(!view){view={follow:saved?.follow??true,frame:0,expected:-1};handlingStreams.set(output,view);handlingStreamSizes.observe(output);}
    const selection=window.getSelection();
    if(selection&&!selection.isCollapsed&&(output.contains(selection.anchorNode)||output.contains(selection.focusNode))){pauseHandlingStream(output,view);view.selectionPaused=true;continue;}
    view.selectionPaused=false;
    const anchor=view.follow?null:[...output.querySelectorAll('.handling-stream-entry:not(.handling-tool-group)')].find(el=>{const r=el.getBoundingClientRect();return r.height>0&&r.bottom>output.getBoundingClientRect().top;}),anchorTop=anchor?.getBoundingClientRect().top;
    const entries=groupHandlingEntries(handlingConversationEntries(run));
    // Reuse single records when a second tool creates a group around them.
    const nodes=new Map([...output.querySelectorAll('[data-stream-entry]')].map(el=>[el.dataset.streamEntry,el])),keys=new Set();
    renderHandlingEntries(output,entries,{run,saved,initial,nodes,keys});
    for(const [key,el] of nodes)if(!keys.has(key))el.remove();
    for(const motion of handlingToolMotions.values())if(motion.output===output&&motion.el.isConnected){
      const height=motion.el.style.height;motion.el.style.height='';motion.el.open=motion.open;
      motion.end=motion.el.getBoundingClientRect().height;motion.el.open=true;motion.el.style.height=height;
    }
    if(initial){output.scrollTop=saved?.top||0;if(view.follow)followHandlingStream(output,view,true);}
    else if(anchor?.isConnected&&!view.follow){output.scrollTop+=anchor.getBoundingClientRect().top-anchorTop;view.expected=output.scrollTop;}
    handlingStreamLatest(output,view);followHandlingStream(output,view);
  }
}
const handlingTextKinds=new Set(['agentMessage','userMessage','conclusion','notice','turnLabel']);
function groupHandlingEntries(entries) {
  const grouped=[];
  for(let i=0;i<entries.length;){
    const first=entries[i],batch=[first];i++;
    if(!handlingTextKinds.has(first.kind))while(i<entries.length&&entries[i].kind===first.kind)batch.push(entries[i++]);
    grouped.push(batch.length>1?{key:first.key+':group',kind:'toolGroup',toolKind:first.kind,entries:batch}:first);
  }
  return grouped;
}
function renderHandlingEntries(parent, entries, context, inGroup = false) {
  const { run, saved, initial, nodes, keys } = context;
  let previous = null;
  for (const entry of entries) {
    keys.add(entry.key);
    const group = entry.kind === 'toolGroup',
      detail = !handlingTextKinds.has(entry.kind),
      user = entry.kind === 'userMessage';
    let el = nodes.get(entry.key);
    if (!el && group) {
      // Older records can arrive before a group. Reuse its container by a
      // surviving member so disclosure state and any animation stay intact.
      el = entry.entries
        .map(e => nodes.get(e.key)?.closest('.handling-tool-group'))
        .find(g => g && !keys.has(g.dataset.streamEntry));
      if (el) {
        nodes.delete(el.dataset.streamEntry);
        el.dataset.streamEntry = entry.key;
      }
    }
    if (el && el.dataset.streamKind !== entry.kind) {
      el.remove();
      el = null;
    }
    if (!el) {
      el = document.createElement(detail ? 'details' : 'section');
      el.dataset.streamEntry = entry.key;
      el.dataset.streamKind = entry.kind;
      if (detail) {
        const title = document.createElement('summary');
        title.innerHTML = '<span class="handling-tool-icon" aria-hidden="true"></span><span class="handling-tool-label"></span><span class="handling-tool-chevron" aria-hidden="true">›</span>';
        el.append(title);
      }
      const body = document.createElement('div');
      body.className = group ? 'handling-tool-children' : entry.kind === 'turnLabel' ? 'handling-turn-label' : 'handling-stream-text';
      el.append(body);
      if (detail && (saved?.opened?.has(entry.key) || (group && entry.entries.some(e => nodes.get(e.key)?.open)))) {
        el.open = true;
      }
    }
    const inserted = !el.isConnected;
    el.className = 'handling-stream-entry'
      + (detail ? ' handling-stream-detail' : user ? ' handling-user-message' : ' handling-assistant-message')
      + (group ? ' handling-tool-group' : '')
      + (inGroup ? ' handling-tool-child' : '');
    if (previous ? previous.nextElementSibling !== el : parent.firstElementChild !== el) {
      if (previous) previous.after(el);
      else parent.prepend(el);
    }
    previous = el;
    if (inserted && !initial) motionAnimate(el, [{ opacity: .35 }, { opacity: 1 }]);
    if (group) {
      renderHandlingEntries(el.querySelector('.handling-tool-children'), entry.entries, context, true);
      el.querySelector('.handling-tool-label').textContent = handlingToolLabel({ kind: entry.toolKind }) + ' · ' + entry.entries.length;
      el.querySelector('.handling-tool-icon').innerHTML = handlingToolIcon(entry.toolKind);
      continue;
    }
    const body = el.querySelector('.handling-stream-text,.handling-turn-label'),
      raw = String(entry.text || ''),
      text = detail ? raw : raw.trim(),
      shorterSnapshot = handlingActive(run) && body._handlingText?.startsWith(text) && body._handlingText.length > text.length;
    if (!shorterSnapshot && body._handlingText !== text) {
      // Display received snapshots immediately; do not replay invented streaming.
      body._handlingText = text;
      if (['conclusion', 'agentMessage'].includes(entry.kind)) body.innerHTML = handlingMd(text);
      else body.textContent = text;
    }
    if (detail) {
      el.querySelector('.handling-tool-label').textContent = handlingToolLabel(entry, inGroup);
      el.querySelector('.handling-tool-icon').innerHTML = handlingToolIcon(entry.kind);
    }
  }
}

document.addEventListener('scroll',e=>{
  if(!e.target.matches?.('[data-handling-stream]'))return;
  const output=e.target,view=handlingStreams.get(output);if(!view||Math.abs(output.scrollTop-view.expected)<2)return;
  view.follow=handlingStreamAtLatest(output);if(!view.follow)pauseHandlingStream(output,view);else handlingStreamLatest(output,view);
},true);
document.addEventListener('wheel',e=>{
  const output=e.target.closest?.('[data-handling-stream]'),view=output&&handlingStreams.get(output);
  if(view&&e.deltaY<0)pauseHandlingStream(output,view);
},{passive:true});
document.addEventListener('keydown',e=>{
  const output=e.target.closest?.('[data-handling-stream]'),view=output&&handlingStreams.get(output);
  if(view&&['ArrowUp','PageUp','Home'].includes(e.key))pauseHandlingStream(output,view);
});
document.addEventListener('click',e=>{
  const button=e.target.closest('[data-handling-latest]');if(!button)return;
  const output=button.parentElement.querySelector('[data-handling-stream]'),view=handlingStreams.get(output);
  if(view){view.follow=true;followHandlingStream(output,view);button.hidden=true;}
});
document.addEventListener('selectionchange',()=>{
  if(!window.getSelection()?.isCollapsed)return;
  if([...document.querySelectorAll('[data-handling-stream]')].some(el=>handlingStreams.get(el)?.selectionPaused))updateHandlingLive();
});
function handlingConversationEntries(run) {
  const entries=[],rounds=run.rounds||[];
  for(const [index,round] of rounds.entries()) {
    const prefix=String(round.id||round.turn_id||index),steps=(round.steps||[]).filter(s=>s.text);
    if(rounds.length>1)entries.push({key:prefix+':label',kind:'turnLabel',text:index===rounds.length-1?'本轮':index===rounds.length-2?'上一轮':`第 ${index+1} 轮`});
    if(round.question && !steps.some(s=>s.kind==='userMessage' && String(s.text).trim()===round.question.trim())) entries.push({key:prefix+':question',kind:'userMessage',text:round.question});
    for(const [i,step] of steps.entries()) entries.push({...step,key:prefix+':step:'+String(step.id??i)});
    for(const [i,input] of (round.supplements||[]).entries()) {
      if(input.text && !steps.some(s=>s.kind==='userMessage' && String(s.text).trim()===input.text.trim())) entries.push({key:prefix+':supplement:'+String(input.id||i),kind:'userMessage',text:input.text+(input.not_submitted?'\n（未发送）':input.accepted?'':'\n（发送状态待核实）')});
    }
    if(round.conclusion && !steps.some(s=>s.kind==='agentMessage'&&String(s.text).trim()===round.conclusion.trim())) entries.push({key:prefix+':conclusion',kind:'conclusion',text:round.conclusion});
    if(!steps.length && !round.conclusion && index===rounds.length-1 && run.progress) entries.push({key:prefix+':progress',kind:'agentMessage',text:run.progress});
  }
  if(!rounds.length && run.progress) entries.push({key:'progress',kind:'agentMessage',text:run.progress});
  if(!entries.length || (handlingState(run)==='completed' && !rounds.at(-1)?.conclusion && !rounds.at(-1)?.steps?.some(s=>s.kind==='agentMessage'))) entries.push({key:'notice',kind:'notice',text:handlingState(run)==='completed'?'本轮已结束，执行端未提供最终文字结论。':'等待执行端同步进展…'});
  return entries;
}
function handlingToolIcon(kind) {
  const paths=['commandExecution','functionCall','functionCallOutput'].includes(kind)?'<rect x="3" y="3" width="18" height="18" rx="3"/><path d="m7 8 4 4-4 4m6 0h4"/>':'<circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3c5 5 5 13 0 18-5-5-5-13 0-18"/>';
  return `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">${paths}</svg>`;
}
function handlingToolLabel(entry,inGroup=false) {
  const label={commandExecution:'执行命令',fileChange:'文件变更',mcpToolCall:'调用工具',webSearch:'网页检索',functionCall:'执行工具',functionCallOutput:'工具输出'}[entry.kind]||'处置进展';
  const first=String(entry.text||'').trim().split('\n')[0].trim();
  // Keep raw commands and outputs in the disclosure, not in its one-line label.
  if(inGroup){
    const preview=entry.kind==='commandExecution'?first.replace(/^(?:&\s*)?(?:"([^"]+)"|'([^']+)'|(\S+))/,(_,double,single,bare)=>(double||single||bare).split(/[\\/]/).pop()):entry.kind==='fileChange'?first.split(/[\\/]/).pop():first;
    return (preview||label).slice(0,120);
  }
  const detail=entry.kind==='fileChange'?first.split(/[\\/]/).pop():['mcpToolCall','functionCall'].includes(entry.kind)?first:'';
  return label+(detail?' · '+detail.slice(0,80):'');
}
