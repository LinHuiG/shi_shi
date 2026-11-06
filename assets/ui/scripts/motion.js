// Short, interruptible motion. Data and controls update synchronously.
const motionDuration = 140,
  motionAnimations = new WeakMap();
const motionReduced = () => matchMedia('(prefers-reduced-motion: reduce)').matches;
function motionAnimate(el, frames, options = {}) {
  if (!el?.isConnected) return;
  motionAnimations.get(el)?.cancel();
  if (motionReduced() || typeof el.animate !== 'function') return;
  const animation = el.animate(frames, {
    duration: motionDuration,
    easing: 'cubic-bezier(.2,0,0,1)',
    ...options,
  });
  motionAnimations.set(el, animation);
  const done = () => {
    if (motionAnimations.get(el) === animation) motionAnimations.delete(el);
  };
  animation.finished.then(done, done);
  return animation;
}
function motionText(el, value, slide = false) {
  if (!el) return;
  const text = String(value ?? '');
  if (el.textContent === text) return;
  const had = !!el.textContent;
  el.textContent = text;
  if (had)
    motionAnimate(el, [
      { opacity: 0.45, transform: slide ? 'translateY(4px)' : 'none' },
      { opacity: 1, transform: 'none' },
    ]);
}
function motionExit(el) {
  if (!el) return;
  // A visual copy cannot keep stale controls, IDs or event targets alive.
  if (!motionReduced() && el.isConnected && el.getBoundingClientRect().height) {
    const rect = el.getBoundingClientRect(),
      copy = el.cloneNode(true),
      original = [el, ...el.querySelectorAll('*')],
      copies = [copy, ...copy.querySelectorAll('*')];
    const props = [
      'display',
      'box-sizing',
      'font',
      'color',
      'background',
      'border',
      'border-radius',
      'padding',
      'gap',
      'align-items',
      'justify-content',
      'flex-direction',
      'grid-template-columns',
      'white-space',
      'overflow',
      'text-align',
    ];
    original.slice(0, 120).forEach((node, i) => {
      const style = getComputedStyle(node);
      for (const prop of props) copies[i].style.setProperty(prop, style.getPropertyValue(prop));
    });
    for (const node of copies) {
      for (const attr of [...node.attributes])
        if (attr.name === 'id' || attr.name === 'class' || attr.name === 'name' || attr.name.startsWith('data-'))
          node.removeAttribute(attr.name);
      if (node.matches('input,textarea,select,button')) node.disabled = true;
    }
    copy.setAttribute('aria-hidden', 'true');
    copy.inert = true;
    copy.dataset.motionGhost = 'true';
    copy.style.cssText += `;position:fixed;left:${rect.left}px;top:${rect.top}px;width:${rect.width}px;height:${rect.height}px;margin:0;pointer-events:none;z-index:100;overflow:hidden;`;
    document.body.append(copy);
    const a = motionAnimate(copy, [
      { opacity: 1, transform: 'none' },
      { opacity: 0, transform: 'translateY(-3px)' },
    ]);
    if (a)
      a.finished.then(
        () => copy.remove(),
        () => copy.remove(),
      );
    else copy.remove();
  }
  el.remove();
}
function motionHide(el) {
  if (!el) return;
  const parent = el.parentNode,
    next = el.nextSibling;
  motionExit(el);
  el.hidden = true;
  parent?.insertBefore(el, next);
}
function motionKey(el) {
  if (el.nodeType !== 1) return '';
  for (const name of [
    'data-ui-key',
    'id',
    'data-task',
    'data-message-key',
    'data-event-key',
    'data-report-key',
    'data-advice-card',
    'data-filter-group',
    'data-ai-connection',
    'data-view',
    'data-page',
    'data-handle',
    'data-action',
    'data-prompt-section',
  ])
    if (el.hasAttribute(name)) return el.tagName + ':' + name + ':' + el.getAttribute(name);
  return '';
}
function reconcileHtml(host, html, { animate = true } = {}) {
  animate = animate && !motionReduced();
  if (!animate)
    for (const animation of host.getAnimations({ subtree: true }))
      if (motionAnimations.get(animation.effect?.target) === animation) animation.cancel();
  const template = document.createElement('template');
  template.innerHTML = html;
  const active = document.activeElement,
    selection = active?.matches('input,textarea')
      ? [active.selectionStart, active.selectionEnd, active.selectionDirection]
      : null;
  const rowSelector = '.task-row,.inbox-row,.event,.advice-card,.report-card';
  const positions = new Map(
    animate ? [...host.querySelectorAll(rowSelector)].map((el) => [el, el.getBoundingClientRect()]) : [],
  );
  for (const el of positions.keys()) motionAnimations.get(el)?.cancel();
  const scrollers = [host, ...host.querySelectorAll('.task-list,.detail,.conclusion-body,.ai-live-output')].filter(
    (el) => el.scrollHeight > el.clientHeight + 1,
  );
  const anchors = scrollers.map((el) => {
    const rect = el.getBoundingClientRect(),
      node = [...el.children].find((n) => n.getBoundingClientRect().bottom > rect.top);
    return { el, node, top: node?.getBoundingClientRect().top, scroll: el.scrollTop };
  });
  function patch(old, next) {
    if (old.nodeType !== 1) {
      if (old.nodeValue !== next.nodeValue) {
        old.nodeValue = next.nodeValue;
        const parent = old.parentElement;
        if (
          animate &&
          parent?.matches('.badge,.task-status,.advice-status,.phase-copy,.side-pending,.usage-line b,.report-counts b')
        )
          motionAnimate(parent, [{ opacity: 0.45 }, { opacity: 1 }]);
      }
      return;
    }
    const focused = old === active,
      open = old.tagName === 'DETAILS' ? old.open : null,
      value = old.matches('input,textarea,select') ? old.value : null,
      nextValue = value !== null ? next.value : null,
      previous = old.getAttribute('value');
    for (const attr of [...old.attributes])
      if (
        !next.hasAttribute(attr.name) &&
        attr.name !== 'open' &&
        !(attr.name === 'style' && motionAnimations.has(old))
      )
        old.removeAttribute(attr.name);
    for (const attr of [...next.attributes])
      if (attr.name !== 'open' && old.getAttribute(attr.name) !== attr.value) old.setAttribute(attr.name, attr.value);
    if (!old.hasAttribute('data-preserve-content')) children(old, next);
    if (open !== null) old.open = open;
    if (value !== null) {
      if (focused) old.value = value;
      else if (old.tagName !== 'INPUT' || previous !== next.getAttribute('value')) old.value = nextValue;
    }
  }
  function children(parent, next) {
    const remaining = new Set(parent.childNodes),
      candidates = new Map();
    // Unkeyed nodes match in document order; a duplicate explicit key keeps
    // the last old node, just as the previous keyed map did.
    for (const node of [...remaining].reverse()) {
      const key = motionKey(node),
        slot = key || node.nodeType + ':' + node.nodeName;
      if (!candidates.has(slot)) candidates.set(slot, []);
      const group = candidates.get(slot);
      if (!key || !group.length) group.push(node);
    }
    let cursor = parent.firstChild;
    for (const desired of [...next.childNodes]) {
      const key = motionKey(desired) || desired.nodeType + ':' + desired.nodeName;
      let node = candidates.get(key)?.pop();
      if (node) {
        remaining.delete(node);
        patch(node, desired);
        if (node !== cursor) parent.insertBefore(node, cursor);
      } else {
        node = desired;
        parent.insertBefore(node, cursor);
        if (animate && node.nodeType === 1 && parent.isConnected)
          motionAnimate(node, [{ opacity: 0.4 }, { opacity: 1 }]);
      }
      cursor = node.nextSibling;
    }
    // Removed results must disappear with the update, not linger over the new list.
    for (const node of remaining) node.remove();
  }
  children(host, template.content);
  for (const { el, node, top, scroll } of anchors)
    if (el.isConnected) {
      if (node?.isConnected) el.scrollTop += node.getBoundingClientRect().top - top;
      else el.scrollTop = scroll;
    }
  for (const [el, before] of positions)
    if (el.isConnected) {
      const after = el.getBoundingClientRect(),
        dy = before.top - after.top;
      if (Math.abs(dy) > 0.5) motionAnimate(el, [{ transform: `translateY(${dy}px)` }, { transform: 'none' }]);
    }
  if (active?.isConnected && document.activeElement !== active) active.focus({ preventScroll: true });
  if (selection && active?.isConnected && selection[0] != null) active.setSelectionRange(...selection);
}
// Native disclosures keep their semantics; only their brief size change animates.
document.addEventListener(
  'click',
  (e) => {
    const trigger = e.target.closest('summary,[data-advice-reply],.tag-group-title');
    if (!trigger) return;
    if (trigger.closest('.record-range')) return;
    const box = trigger.closest('details,.advice-card,.tag-group');
    if (!box) return;
    const height = box.getBoundingClientRect().height;
    motionAnimations.get(box)?.cancel();
    requestAnimationFrame(() => {
      if (!box.isConnected) return;
      const next = box.getBoundingClientRect().height;
      if (Math.abs(next - height) > 1)
        motionAnimate(box, [
          { height: height + 'px', overflow: 'hidden' },
          { height: next + 'px', overflow: 'hidden' },
        ]);
    });
  },
  true,
);
new MutationObserver((records) => {
  for (const record of records) {
    if (record.type === 'childList')
      for (const el of record.addedNodes)
        if (
          el.nodeType === 1 &&
          !el.hasAttribute('data-motion-ghost') &&
          el.matches('.palette,.backlog-panel,.work-state-panel,.help-popover')
        )
          motionAnimate(el, [
            { opacity: 0, transform: 'translateY(3px)' },
            { opacity: 1, transform: 'none' },
          ]);
    if (
      record.type === 'attributes' &&
      record.attributeName === 'hidden' &&
      !record.target.hidden &&
      !record.target.hasAttribute('data-closing') &&
      record.target.matches('#overlay,#toast,.advice-composer')
    )
      motionAnimate(record.target, [{ opacity: 0 }, { opacity: 1 }]);
  }
}).observe(document.body, {
  childList: true,
  subtree: true,
  attributes: true,
  attributeFilter: ['hidden'],
});
new MutationObserver((records) => {
  const changed = new Set();
  for (const r of records) {
    const el = r.target.nodeType === 1 ? r.target : r.target.parentElement;
    if (!el?.isConnected || el.closest('[data-motion-ghost],.ai-live-output,[data-live-line],input,textarea,select'))
      continue;
    const target = el.closest(
      '#page-subtitle,#nav-count,.side-pending,.usage-line b,.report-counts b,.advice-status,.summary-lead,.memory-prose,#connection-status,#connection-models,#connection-candidates,#source-candidates,#settings-save-status,#plugin-drop-hint,#inbox-summary',
    );
    if (
      target &&
      ((r.type === 'characterData' && r.oldValue !== r.target.data) ||
        (r.type === 'childList' && [...r.addedNodes].some((n) => n.nodeType === 3)))
    )
      changed.add(target);
  }
  for (const el of changed) motionAnimate(el, [{ opacity: 0.5 }, { opacity: 1 }]);
}).observe(document.body, {
  subtree: true,
  characterData: true,
  characterDataOldValue: true,
  childList: true,
});
