// This small client runs only in the native approval WebView.
const esc=value=>String(value??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
let approvalSequence=0,approvalCurrent=null,approvalSubmitting=false;
const approvalPending=new Map();
function approvalRpc(method,params={}){return new Promise((resolve,reject)=>{const id=++approvalSequence,timer=setTimeout(()=>{approvalPending.delete(id);reject(Error('操作结果尚未确认，请在主界面核对。'));},30000);approvalPending.set(id,{resolve,reject,timer});window.ipc.postMessage(JSON.stringify({id,method,params}));});}
window.receive=reply=>{const pending=approvalPending.get(reply.id);if(!pending)return;clearTimeout(pending.timer);approvalPending.delete(reply.id);reply.ok?pending.resolve(reply.data):pending.reject(Error(reply.error));};
window.setApproval=value=>{
 if(!value){approvalCurrent=null;document.querySelector('#dialog').replaceChildren();return;}
 if(approvalCurrent?.key===value.key&&JSON.stringify(approvalCurrent.request)===JSON.stringify(value.request))return;
 const same=approvalCurrent?.key===value.key,answers=same?[...document.querySelectorAll('[data-handling-answer]')].map(el=>[el.dataset.handlingAnswer,el.value]):[];
 approvalCurrent=value;
 document.querySelector('#dialog').innerHTML=approvalDialog(value.run,value.task,value.request,false);
 for(const [index,text] of answers){const field=document.querySelector(`[data-handling-answer="${index}"]`);if(field)field.value=text;}
 document.querySelectorAll('button').forEach(el=>el.disabled=approvalSubmitting);
 if(window.approvalSmoke)requestAnimationFrame(()=>{const footer=document.querySelector('.approval-footer'),scroll=document.querySelector('.approval-scroll');void approvalRpc('approval_presented',{rendered:!!document.querySelector('[data-decision="accept"]'),footer_visible:!!footer&&footer.getBoundingClientRect().bottom<=innerHeight,scrollable:!!scroll&&scroll.scrollHeight>scroll.clientHeight}).catch(()=>{});});
};
document.addEventListener('change',event=>{const input=event.target;if(input.matches('[data-approval-option]')){const field=document.querySelector(`[data-handling-answer="${input.dataset.approvalOption}"]`);if(field)field.value=input.value;}});
document.addEventListener('input',event=>{const input=event.target;if(input.matches('[data-handling-answer]'))document.querySelectorAll(`[name="approval-choice-${input.dataset.handlingAnswer}"]`).forEach(option=>option.checked=option.value===input.value);});
document.addEventListener('keydown',event=>{if(event.key==='Escape')document.querySelector('[data-action="close"]')?.click();});
document.addEventListener('click',async event=>{
 const button=event.target.closest('button');if(!button||approvalSubmitting)return;
 if(button.dataset.action==='close'){await approvalRpc('approval_hide',{key:approvalCurrent?.key}).catch(()=>{});return;}
 if(button.dataset.handle!=='answer'||!approvalCurrent)return;
 const current=approvalCurrent,answers={};
 for(const [i,question] of (current.request.params?.questions||[]).entries())answers[question.id]=document.querySelector(`[data-handling-answer="${i}"]`)?.value||'';
 approvalSubmitting=true;document.querySelectorAll('button').forEach(el=>el.disabled=true);
 try{await approvalRpc('handling_request',{task_id:current.run.task_id,run_id:current.run.id,thread_id:current.run.thread_id,request_id:JSON.stringify(current.request.id),action:button.dataset.decision,option_id:button.dataset.optionId,answers});}
 catch(error){if(approvalCurrent?.key===current.key){const field=document.querySelector('#modal-error');if(field)field.textContent=error.message;}}
 finally{approvalSubmitting=false;document.querySelectorAll('button').forEach(el=>el.disabled=false);}
});
void approvalRpc('approval_ready').catch(()=>{});
