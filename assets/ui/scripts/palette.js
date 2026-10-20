// Command palette (Ctrl+K) and global shortcuts. Every command runs through the
// same page navigation and visible buttons a mouse user would use, so no
// action gains a second, divergent code path. Shortcuts are listed in the
// palette and in sidebar tooltips so they can be learned in place.
const pageOrder=['tasks','schedules','inbox','reports','sources','prompts','logs','settings'];
const go=async target=>{if(page!==target){if(!await leaveSettings())return false;page=target;await render();}return true;};
const runAction=async(target,action)=>{if(!await go(target))return;document.querySelector(`#top-actions [data-action="${action}"],#view [data-action="${action}"]`)?.click();};
const focusSearch=async()=>{if(!['tasks','inbox'].includes(page)&&!await go('tasks'))return;($('#task-search')||$('#inbox-search'))?.focus();};
const commands=()=>[
 ...pageOrder.map((p,i)=>({label:'前往 '+names[p],keys:`Ctrl ${i+1}`,hint:p,run:()=>go(p)})),
 {label:'新建事项',keys:'Ctrl N',hint:'new task',run:()=>runAction('tasks','new')},
 {label:'搜索事项或消息',keys:'/',hint:'search find',run:focusSearch},
 {label:'刷新状态',keys:'F5',hint:'refresh reload',run:()=>refreshState(true)},
 {label:'刷新并分析',hint:'scan analyze',run:()=>runAction('tasks','scan')},
 {label:'读取新增消息',hint:'inbox refresh messages',run:()=>runAction('inbox','refresh-messages')},
 {label:'分析待处理消息',hint:'analyze pending',run:()=>runAction('inbox','analyze')},
 {label:'新建定时事项',hint:'schedule new',run:()=>runAction('schedules','new-schedule')},
 {label:'手动归档',hint:'archive report',run:()=>runAction('reports','archive')},
];
let paletteIndex=0,paletteRows=[];
function closePalette(){motionExit($('#palette'));}
function renderPalette(){
 const q=($('#palette-input')?.value||'').trim().toLowerCase();
 paletteRows=commands().filter(c=>!q||(c.label+' '+c.hint).toLowerCase().includes(q));
 paletteIndex=Math.min(paletteIndex,Math.max(0,paletteRows.length-1));
 setHtml('#palette-list',paletteRows.map((c,i)=>`<li role="option" id="palette-${i}" aria-selected="${i===paletteIndex}" data-palette-index="${i}"><span>${esc(c.label)}</span>${c.keys?`<kbd>${esc(c.keys)}</kbd>`:''}</li>`).join('')||'<li class="palette-empty">没有匹配的命令</li>');
 $('#palette-input')?.setAttribute('aria-activedescendant',paletteRows.length?'palette-'+paletteIndex:'');
 $('#palette-'+paletteIndex)?.scrollIntoView({block:'nearest'});
}
function openPalette(){
 if($('#palette')){closePalette();return;}
 if(!leaveConnectionEditor())return;
 const panel=document.createElement('div');panel.id='palette';panel.className='palette';panel.setAttribute('role','dialog');panel.setAttribute('aria-label','命令面板');
 panel.innerHTML='<input id="palette-input" role="combobox" aria-expanded="true" aria-controls="palette-list" autocomplete="off" placeholder="输入命令，例如：新建、收件箱、归档"><ul id="palette-list" role="listbox"></ul>';
 document.body.append(panel);paletteIndex=0;renderPalette();$('#palette-input').focus();
}
async function runPalette(i){const c=paletteRows[i];if(!c)return;closePalette();try{await c.run();}catch(e){toast(e.message);}}
document.addEventListener('input',e=>{if(e.target.id==='palette-input'){paletteIndex=0;renderPalette();}});
document.addEventListener('mousedown',e=>{const row=e.target.closest('[data-palette-index]');if(row){e.preventDefault();runPalette(Number(row.dataset.paletteIndex));}else if($('#palette')&&!e.target.closest('#palette'))closePalette();});
document.addEventListener('keydown',e=>{
 if(e.isComposing)return;
 const key=e.key.toLowerCase(),ctrl=e.ctrlKey||e.metaKey,modal=!$('#overlay').hidden;
 if(ctrl&&key==='k'){e.preventDefault();if(!modal)openPalette();return;}
 if($('#palette')){
  if(key==='escape'){e.preventDefault();e.stopImmediatePropagation();closePalette();}
  else if(key==='arrowdown'||key==='arrowup'){e.preventDefault();const n=paletteRows.length;if(n){paletteIndex=(paletteIndex+(key==='arrowdown'?1:n-1))%n;renderPalette();}}
  else if(key==='enter'){e.preventDefault();runPalette(paletteIndex);}
  return;
 }
 // A page reload would navigate away from the embedded document; refresh data instead.
 if(key==='f5'||(ctrl&&key==='r')){e.preventDefault();refreshState(true);return;}
 if(modal)return;
 const typing=e.target.closest?.('input,textarea,select,[contenteditable="true"]');
 if(ctrl&&!e.shiftKey&&!e.altKey&&/^[1-8]$/.test(key)){e.preventDefault();go(pageOrder[Number(key)-1]);return;}
 if(ctrl&&key==='n'){e.preventDefault();runAction('tasks','new');return;}
 if(ctrl&&key==='s'&&page==='settings'){e.preventDefault();void flushSettings();return;}
 if(!typing&&!ctrl&&!e.altKey&&key==='/'){e.preventDefault();focusSearch();}
},true);
for(const [i,p] of pageOrder.entries()){const b=document.querySelector(`aside [data-page="${p}"]`);if(b){b.title=`${names[p]}（Ctrl+${i+1}）`;b.setAttribute('aria-keyshortcuts',`Control+${i+1}`);}}
document.querySelector('#palette-open')?.addEventListener('click',openPalette);
