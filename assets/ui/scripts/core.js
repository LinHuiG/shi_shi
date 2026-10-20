
let uiReady=false;let seq=0;const pending=new Map();function rpcRaw(method,params={}){return new Promise((resolve,reject)=>{const id=++seq;const timer=setTimeout(()=>{pending.delete(id);reject(Error('请求超时，请查看日志；不要重复提交同一操作。'))},300000);pending.set(id,{resolve,reject,timer});window.ipc.postMessage(JSON.stringify({id,method,params}))})}window.receive=v=>{const p=pending.get(v.id);if(!p)return;clearTimeout(p.timer);pending.delete(v.id);v.ok?p.resolve(v.data):p.reject(Error(v.error))};
const longAiMethods=new Set(['scan','analyze','format','schedule_format','edit_ai','report','archive','learn','source_range','prompt_retry','handling_learn']);
async function rpc(method,params={}){
 if(!longAiMethods.has(method)||!state?.async_ai_requests)return rpcRaw(method,params);
 const accepted=await rpcRaw('ai_job_start',{method,params});if(!accepted?.job_id)throw Error('后台操作未返回编号，请查看日志；不要重复提交。');
 for(;;){
  let result;try{result=await rpcRaw('ai_job_result',{job_id:accepted.job_id});}catch(error){throw Error(`查询后台操作 ${accepted.job_id} 失败：${error.message}；后台可能仍在执行，请查看状态与日志。`);}
  if(result.done){if(!result.ok)throw Error(result.error);return result.data;}
  await new Promise(resolve=>setTimeout(resolve,750));
 }
}
const $=s=>document.querySelector(s);const esc=s=>String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
function recordedAiSource(detail){if(detail?.source)return detail.source;let route;try{route=JSON.parse(detail?.model)}catch{}return Array.isArray(route)&&route.length===2&&route.every(v=>typeof v==='string')?{driver:'dsh',connection_name:'DSH（历史记录）',provider:route[0],model_name:route[1],label:'DSH（历史记录） / '+route[0]}:null;}
function aiSourceLabel(source){return source?.label||source?.connection_name||'来源未记录';}
function aiModelLabel(detail){const source=recordedAiSource(detail);let route;try{route=JSON.parse(detail.model)}catch{}const provider=source?.provider||(Array.isArray(route)?route[0]:'');const model=source?.model_name||(Array.isArray(route)?route[1]:detail.model)||'默认模型';return `${aiSourceLabel(source)}${!source?.provider&&provider?' / '+provider:''} · ${model}`;}
function analysisProgressText(){const live=state?.ai_live,text=(live?.entries||[]).at(-1)?.text||live?.phase||state?.ai_progress||'正在连接，等待公开内容';return Array.from(String(text).split(/\r?\n/).map(s=>s.trim()).filter(Boolean).at(-1)||'').slice(-260).join('')||'等待公开内容';}
const date=t=>t?new Date(t).toLocaleString('zh-CN',{timeZone:'Asia/Shanghai',hour12:false}):'尚未扫描';
let state=null,page='tasks',selected='',tab='今日事项',query='',inboxQuery='',settingsDirty=false,modalTask=null,manualSources=[],toastTimer,evidenceCache={},unusedPlaceholder=null,modalFromTrace=false;
const names={tasks:'事项',schedules:'定时事项',inbox:'消息收件箱',reports:'报表与归档',learning:'迭代建议',logs:'日志与操作',settings:'设置',sources:'消息来源',prompts:'系统提示词',guide:'使用说明'};
function toast(s){$('#toast').textContent=s;$('#toast').hidden=false;clearTimeout(toastTimer);motionAnimate($('#toast'),[{opacity:.3},{opacity:1}]);toastTimer=setTimeout(()=>motionHide($('#toast')),6000)}
function dueLabel(t){if(!t.due)return '';const d=new Date(t.due);if(!Number.isFinite(d.getTime()))return t.due;const hours=(d-Date.now())/3600000;return (hours<0?'已逾期 · ':hours<24?'即将截止 · ':'截止 ')+date(d.getTime())}function localDue(d){return d?new Date(new Date(d).getTime()+8*3600000).toISOString().slice(0,16):''}function badge(p){return `<span class="badge ${p==='紧急'?'urgent':p==='高'?'high':p==='已完成'?'green':p==='普通'?'normal':p==='低'?'low':''}">${esc(p)}</span>`}
function empty(title,text,button=''){return `<div class="empty"><div class="empty-icon">▤</div><div>${title}</div><p>${text}</p>${button}</div>`}
function setModal(html){modalFromTrace=false;$('#dialog').classList.remove('wide-dialog','live-dialog','approval-shell');$('#dialog').innerHTML=html;$('#overlay').hidden=false;$('#dialog').querySelector('textarea,input')?.focus({preventScroll:true});motionAnimate($('#dialog'),[{opacity:.4,transform:'translateY(4px)'},{opacity:1,transform:'none'}])}
function closeModal(){if($('#dialog').querySelector('[data-pending="true"]'))return;motionHide($('#overlay'));modalTask=null;modalFromTrace=false;}
async function perform(method,params={},notice='已完成'){try{const v=await rpc(method,params);await refreshState(true);if(notice)toast(notice);return v}catch(e){toast(e.message);throw e}}
const htmlMemo=new WeakMap();
function setHtml(selector,html){const el=typeof selector==='string'?$(selector):selector;if(!el)return;const previous=htmlMemo.get(el);if(previous?.input===html&&previous.dom===el.innerHTML)return;reconcileHtml(el,html);htmlMemo.set(el,{input:html,dom:el.innerHTML});}
let refreshInFlight=null,refreshForced=false,refreshQueued=false,renderedPage='',renderedKey='',renderEpoch=0,pluginCatalogRevision=0;
function viewKey(s,p){switch(p){
 case 'tasks':return JSON.stringify([s.tag_state,(s.handling||[]).map(({synced,updated,progress,...r})=>({...r,rounds:r.rounds?.map(({steps,...round})=>round)})),s.tasks,s.messages,s.last_analysis_success_at,s.usage.day,s.pending,s.config.enabled,s.ai_busy,s.busy]);
 case 'inbox':return JSON.stringify([s.messages_revision??s.messages,s.pending,s.last_scan,s.config.enabled,s.config.interval_minutes,s.config.message_retention_days]);
 case 'schedules':return JSON.stringify(s.schedules);
 case 'reports':return JSON.stringify([s.reports,s.config.work_start,s.config.work_end,s.archive_attempt]);
 case 'learning':return JSON.stringify([s.suggestions,s.rule_version,s.tag_state,s.evolution]);
 case 'logs':return JSON.stringify([s.audit,s.activity,s.usage]);
 case 'sources':return JSON.stringify([s.chat_sources,s.disabled_plugins,pluginCatalogRevision]);
 case 'prompts':return JSON.stringify([promptSection,promptSection==='advice'?[s.suggestions,s.rule_version,s.tag_state,s.evolution]:promptSection==='memory'?s.memory_state:s.prompt_state]);
 case 'handling':return JSON.stringify([s.handling_version,s.handling_proposals]);
 default:return '';
}}
window.refreshState=function(force=false){
 if(refreshInFlight){if(force){refreshForced=true;refreshQueued=true;}return refreshInFlight;}
 refreshForced=force;
 refreshInFlight=(async()=>{do{refreshQueued=false;try{
  const forced=refreshForced;refreshForced=false;
  const next=await rpc('state',{page});if(next.tag_state?.unified&&JSON.stringify(next.tag_state.groups)!==JSON.stringify(state?.tag_state?.groups)){selectedTags=structuredClone(next.tag_state.filters||{});selectedPriorities=[];}state={reports:[],audit:[],messages:[],...state,...next};status();updateHandlingLive();
  const active=document.activeElement,editing=$('#view').contains(active)&&active?.matches('input,textarea,select')&&active.id!=='inbox-search';
  const changed=renderedPage!==page||renderedKey!==viewKey(state,page);
  if((forced&&!(page==='settings'&&renderedPage==='settings'&&(settingsChanged()||settingsSavePromise||settingsQueue||connectionEditor)))||(changed&&page!=='settings'&&(!editing||page==='tasks')&&!settingsDirty&&$('#overlay').hidden)){
   const target=page,scroll=$('#view').scrollTop;await render();if(page===target)$('#view').scrollTop=scroll;
  }
  if(!uiReady){uiReady=true;await rpc('ui_ready',{rendered:!!$('#page-title')?.textContent&&!!$('#view')?.children.length,tag_catalog_ready:Array.isArray(state.tag_state?.groups),task_rows:document.querySelectorAll('.task-row').length,plugin_drop:typeof installPluginFile==='function'});}
 }catch(e){toast(e.message);}}while(refreshQueued);})().finally(()=>{refreshInFlight=null;});
 return refreshInFlight;
};
let modelCatalog=[],modelRequest=0,settingsBaseline={},scanToggleBusy=false;
function readSettingsValues(){return Object.fromEntries([...document.querySelectorAll('#view [id^="cfg-"]')].filter(el=>el.matches('input,textarea,select')).map(el=>[el.id,el.type==='checkbox'?el.checked:el.type==='number'?Number(el.value):el.id==='cfg-leaders'?el.value.split(/[\n,，]/).map(s=>s.trim()).filter(Boolean).join('\n'):el.value.trim()]));}
function settingsChanged(){if(page!=='settings')return settingsDirty;const current=readSettingsValues();return Object.keys(settingsBaseline).some(id=>current[id]!==settingsBaseline[id]);}
async function leaveSettings(){if(typeof leaveMemory==='function'&&memoryEditor&&!leaveMemory())return false;if(connectionEditor?.saving||connectionEditor?.busy){toast(connectionEditor.saving?'正在保存连接':'正在读取连接，请稍后重试');restoreConnectionFocus();return false;}if(page==='settings'&&!await flushSettings()){toast('设置尚未保存，请修正输入或重试');return false;}if(connectionDirty()&&!confirm('连接修改尚未保存，放弃修改？')){restoreConnectionFocus();return false;}closeConnectionEditor();settingsDirty=false;return true;}

const effortNames={none:'无',minimal:'最少',low:'低',medium:'中',high:'高',xhigh:'很高',max:'最高',ultra:'超高'};
