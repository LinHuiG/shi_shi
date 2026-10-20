const handlingStreams=new WeakMap();
function handlingLivePanel(r){return `<section class="handling-stream" data-ui-key="stream-${esc(r.id)}"><div class="handling-stream-output" data-handling-stream="${esc(r.id)}" data-preserve-content tabindex="0" aria-label="处置实时进度"></div><div class="handling-stream-footer"><span data-handling-stream-status>${handlingActive(r)?'持续同步进展':'本轮进度'}</span><button class="quiet" data-handling-latest hidden>回到最新 ↓</button></div></section>`;}
function followHandlingStream(output,view){
 if(!view.follow||view.frame)return;const start=performance.now(),top=output.scrollTop;
 const step=now=>{view.frame=0;if(!output.isConnected||!view.follow)return;const end=Math.max(0,output.scrollHeight-output.clientHeight),t=motionReduced()?1:Math.min(1,(now-start)/motionDuration);view.expected=top+(end-top)*(1-(1-t)**3);output.scrollTop=view.expected;view.expected=output.scrollTop;if(t<1&&end-output.scrollTop>1)view.frame=requestAnimationFrame(step);};view.frame=requestAnimationFrame(step);
}
function updateHandlingLive(){
 for(const output of document.querySelectorAll('[data-handling-stream]')){
  const run=(state?.handling||[]).find(r=>r.id===output.dataset.handlingStream);if(!run)continue;
  let view=handlingStreams.get(output);const initial=!view;if(!view){view={follow:true,frame:0,expected:-1,round:''};handlingStreams.set(output,view);}
  const round=run.rounds?.at(-1),roundKey=round?.id||round?.turn_id||run.id;
  if(view.round!==roundKey){output.replaceChildren();view.round=roundKey;view.follow=true;cancelAnimationFrame(view.frame);view.frame=0;}
  const steps=(round?.steps||[]).filter(step=>step.text),entries=steps.length?steps:run.progress?[{id:'progress',kind:'agentMessage',text:run.progress}]:[];
  for(const [index,entry]of entries.entries()){
   const key=String(entry.id??index);let el=[...output.children].find(el=>el.dataset.streamEntry===key);
   if(!el){el=document.createElement('section');el.className='handling-stream-entry';el.dataset.streamEntry=key;const title=document.createElement('h4'),body=document.createElement('div');title.textContent=({agentMessage:'进展说明',commandExecution:'执行命令',fileChange:'文件变更',mcpToolCall:'工具进度',webSearch:'网页检索'})[entry.kind]||'处置进展';body.className='handling-stream-text';el.append(title,body);output.append(el);}
   const body=el.querySelector('.handling-stream-text'),text=String(entry.text||'');
   // A shorter calibration snapshot must not rewind an ongoing visible item.
   const target=smoothTexts.get(body)?.target||body.textContent;
   if(handlingActive(run)&&target.startsWith(text)&&target.length>text.length)continue;
   smoothText(body,text,()=>followHandlingStream(output,view),initial);
  }
  motionText(output.parentElement.querySelector('[data-handling-stream-status]'),handlingActive(run)?'持续同步进展':'本轮进度');
  output.parentElement.querySelector('[data-handling-latest]').hidden=view.follow;followHandlingStream(output,view);
 }
}
document.addEventListener('scroll',e=>{const output=e.target;if(!output.matches?.('[data-handling-stream]'))return;const view=handlingStreams.get(output);if(!view||Math.abs(output.scrollTop-view.expected)<2)return;view.follow=output.scrollHeight-output.clientHeight-output.scrollTop<24;if(!view.follow){cancelAnimationFrame(view.frame);view.frame=0;}output.parentElement.querySelector('[data-handling-latest]').hidden=view.follow;},true);
document.addEventListener('wheel',e=>{const output=e.target.closest('[data-handling-stream]'),view=output&&handlingStreams.get(output);if(view&&e.deltaY<0){view.follow=false;cancelAnimationFrame(view.frame);view.frame=0;view.expected=-1;output.parentElement.querySelector('[data-handling-latest]').hidden=false;}},{passive:true});
document.addEventListener('click',e=>{const button=e.target.closest('[data-handling-latest]');if(!button)return;const output=button.closest('.handling-stream').querySelector('[data-handling-stream]'),view=handlingStreams.get(output);if(view){view.follow=true;followHandlingStream(output,view);button.hidden=true;}});
