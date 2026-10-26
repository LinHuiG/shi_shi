// Tool disclosures expand toward the existing reading anchor. The conversation
// itself is always full-height and has no compact/expanded mode.
const handlingToolMotions=new Map();
function positionHandlingToolMotion(motion) {
  const {output,view}=motion;
  output.scrollTop=motion.follow&&view.follow?handlingStreamEnd(output):motion.top;
  view.expected=output.scrollTop;handlingStreamLatest(output,view);
}
function finishHandlingToolMotion(motion) {
  cancelAnimationFrame(motion.frame);handlingToolMotions.delete(motion.el);
  motion.el.open=motion.open;motion.el.style.height='';motion.el.style.overflow='';
  positionHandlingToolMotion(motion);
}
function toggleHandlingTool(el) {
  const output=el.closest('[data-handling-stream]'),view=handlingStreams.get(output);
  if(!view)return;
  for(const motion of [...handlingToolMotions.values()])if(motion.el!==el&&(motion.el.contains(el)||el.contains(motion.el)))finishHandlingToolMotion(motion);
  const previous=handlingToolMotions.get(el),open=previous?!previous.open:!el.open;
  const follow=handlingStreamAtLatest(output),top=output.scrollTop,height=el.getBoundingClientRect().height;
  if(previous){cancelAnimationFrame(previous.frame);handlingToolMotions.delete(el);}
  cancelAnimationFrame(view.frame);view.frame=0;view.follow=follow;
  el.style.height='';el.open=open;
  const end=el.getBoundingClientRect().height;
  el.open=true;el.style.height=height+'px';el.style.overflow='hidden';
  const motion={el,output,view,open,follow,top,frame:0,start:performance.now(),height,end};
  handlingToolMotions.set(el,motion);positionHandlingToolMotion(motion);
  const step=now=>{
    if(!el.isConnected){cancelAnimationFrame(motion.frame);handlingToolMotions.delete(el);return;}
    const t=motionReduced()?1:Math.min(1,(now-motion.start)/180);
    el.style.height=(height+(motion.end-height)*(1-(1-t)**3))+'px';positionHandlingToolMotion(motion);
    if(t===1)finishHandlingToolMotion(motion);else motion.frame=requestAnimationFrame(step);
  };
  if(motionReduced())step(motion.start+180);else motion.frame=requestAnimationFrame(step);
}
document.addEventListener('click',e=>{
  const summary=e.target.closest('.handling-stream-detail>summary');if(!summary)return;
  e.preventDefault();toggleHandlingTool(summary.parentElement);
},true);
document.addEventListener('toggle',e=>{
  const el=e.target;if(!el.matches?.('.handling-stream-detail')||handlingToolMotions.has(el))return;
  const output=el.closest('[data-handling-stream]'),view=output&&handlingStreams.get(output);
  if(view&&output.isConnected){if(view.follow)followHandlingStream(output,view,true);else handlingStreamLatest(output,view);}
},true);
window.addEventListener('resize',()=>{
  for(const motion of [...handlingToolMotions.values()])finishHandlingToolMotion(motion);
  resizeHandlingStreams();
});
