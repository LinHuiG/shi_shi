// A persistent in-app conversation: only explicit close hides it.
let handlingWindow = null;
const handlingConversationViews = new Map();
function rememberHandlingWindow(p) {
  const output = p.el.querySelector('[data-handling-stream]'), view = output && handlingStreams.get(output);
  if (view) {
    for (const motion of [...handlingToolMotions.values()]) if (motion.output === output) finishHandlingToolMotion(motion);
    handlingConversationViews.set(output.dataset.handlingStream, {
      top: output.scrollTop, follow: view.follow,
      opened: new Set([...output.querySelectorAll('details[open]')].map(el => el.dataset.streamEntry)),
    });
    cancelAnimationFrame(view.frame);
    handlingStreamSizes.unobserve(output);
  }
}
function closeHandlingWindow(focus = false) {
  const p = handlingWindow;
  if (!p) return;
  rememberHandlingWindow(p);
  p.el.remove(); handlingWindow = null;
  if (focus && p.trigger?.isConnected) p.trigger.focus({preventScroll:true});
}
function syncHandlingWindow() {
  const p = handlingWindow;
  if (!p) return;
  const run = taskRun(p.id);
  if (!run) { closeHandlingWindow(); return; }
  const task = state.tasks?.find(t => t.id === p.id) || p.task;
  if (p.run !== run.id) { rememberHandlingWindow(p); p.el.replaceChildren(); p.run = run.id; }
  p.task = task;
  setHtml(p.el, handlingPanel(task));
  resizeHandlingComposers();
}
function openHandlingWindow(id) {
  const run = taskRun(id), task = state.tasks?.find(t => t.id === id);
  if (!run || !task) return;
  if (handlingWindow?.id !== id) {
    closeHandlingWindow();
    const el = document.createElement('section');
    el.id = 'handling-window'; el.className = 'handling-window';
    el.setAttribute('role','dialog'); el.setAttribute('aria-modal','false');
    el.setAttribute('aria-labelledby','handling-window-title');
    handlingWindow = {id,run:run.id,task,el,trigger:document.activeElement};
    document.body.append(el);
    syncHandlingWindow(); updateHandlingLive();
    motionAnimate(el,[{opacity:.3},{opacity:1}]);
  } else { syncHandlingWindow(); updateHandlingLive(); }
  const input = handlingComposerHost(id)?.querySelector('textarea');
  (input || handlingWindow.el.querySelector('[data-handling-close]'))?.focus({preventScroll:true});
}
document.addEventListener('click', e => {
  if (e.target.closest('[data-handling-close]')) closeHandlingWindow(true);
});
document.addEventListener('keydown', e => {
  if (e.key === 'Escape' && !e.isComposing && handlingWindow && $('#overlay').hidden) {
    e.preventDefault(); closeHandlingWindow(true);
  }
}, true);
