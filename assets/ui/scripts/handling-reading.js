// Coordinated reading-card height, inner offset and main-pane position.
const handlingReadingViews = new Map(),
  handlingReadingMotions = new Map(),
  handlingReadingNodes = new WeakSet();
const handlingReadingEdges = new WeakMap(),
  handlingReadingEdgeDelay = 200;
const handlingReadingObserved = new Set(),
  handlingReadingSizes = new ResizeObserver(() => scheduleHandlingReadingMeasure());
const handlingReadingKey = (kind, run, round) =>
  kind + ':' + run.id + ':' + (round?.id || round?.turn_id || 'latest');
function readingView(el) {
  const key = el.dataset.handlingReading;
  if (!handlingReadingViews.has(key)) handlingReadingViews.set(key, { expanded: false, top: 0 });
  return handlingReadingViews.get(key);
}
function handlingReadingPanel(kind, key, body, title = '', attributes = '') {
  const view = handlingReadingViews.get(key),
    motion = handlingReadingMotions.get(key),
    expanded = !!view?.expanded,
    stream = kind === 'stream',
    bodyId = 'handling-reading-' + encodeURIComponent(key);
  const moving = motion ? ` style="height:${motion.height}px;max-height:none"` : '';
  return `<section class="handling-reading ${stream ? 'handling-stream' : 'handling-conclusion'} ${expanded || motion ? 'is-expanded' : ''}" data-ui-key="reading-${esc(key)}" data-handling-reading="${esc(key)}">${title ? `<h4 class="handling-reading-title">${esc(title)}</h4>` : ''}<div class="handling-reading-body ${stream ? 'handling-stream-output' : 'conclusion-body'}" id="${esc(bodyId)}"${moving} ${attributes}>${body}</div><div class="handling-reading-footer ${stream ? 'handling-stream-footer' : 'conclusion-footer'}">${stream ? '<span data-handling-stream-status></span><button class="quiet" data-handling-latest hidden>回到最新 ↓</button>' : ''}<button class="quiet" data-handling-reading-toggle aria-controls="${esc(bodyId)}" aria-expanded="${expanded}" hidden>${expanded ? '收起 ↑' : '展开全部 ↓'}</button></div></section>`;
}
function handlingConclusion(task, r) {
  const round = [...(r.rounds || [])].reverse().find((x) => x.conclusion);
  return round
    ? handlingReadingPanel(
        'conclusion',
        handlingReadingKey('conclusion', r, round),
        handlingMd(round.conclusion),
        round !== r.rounds.at(-1) ? '上轮结论' : 'AI 结论',
      )
    : '';
}
function readingAnchor(body) {
  const el = body.parentElement,
    outer = el.closest('.detail'),
    rect = body.getBoundingClientRect(),
    pane = outer.getBoundingClientRect(),
    footer = el.querySelector('.handling-reading-footer').getBoundingClientRect();
  const top = Math.max(rect.top, pane.top),
    bottom = Math.min(
      rect.bottom,
      pane.bottom,
      el.classList.contains('is-expanded') && footer.top > top ? footer.top : Infinity,
    );
  let y =
      bottom > top
        ? (top + bottom) / 2
        : Math.max(pane.top, Math.min(pane.bottom, rect.top + rect.height / 2)),
    range = null;
  const caret = document.caretRangeFromPoint?.(rect.left + rect.width / 2, y);
  if (
    caret &&
    caret.startContainer.nodeType === Node.TEXT_NODE &&
    body.contains(caret.startContainer) &&
    caret.startContainer.length
  ) {
    range = caret.cloneRange();
    const index = Math.min(range.startOffset, range.startContainer.length - 1);
    range.setStart(range.startContainer, index);
    range.setEnd(range.startContainer, index + 1);
    const text = range.getBoundingClientRect();
    if (text.height && text.top >= top && text.bottom <= bottom) y = (text.top + text.bottom) / 2;
    else range = null;
  }
  const preview = parseFloat(getComputedStyle(body).getPropertyValue('--handling-preview-height'));
  // A glyph near the middle need not sit at the geometric half-pixel. Keep
  // its original compact inset, so a no-scroll round trip does not recenter it.
  return {
    offset: body.scrollTop + y - rect.top,
    y,
    range,
    compactInset: readingView(el).expanded ? preview / 2 : y - rect.top,
  };
}
function readingAnchorOffset(body, anchor) {
  if (anchor.range?.startContainer.isConnected && body.contains(anchor.range.startContainer)) {
    const rect = anchor.range.getBoundingClientRect();
    if (rect.height)
      anchor.offset =
        body.scrollTop + (rect.top + rect.bottom) / 2 - body.getBoundingClientRect().top;
  }
  return anchor.offset;
}
function positionReading(body, anchor, scroll) {
  const outer = body.closest('.detail'),
    view = readingView(body.parentElement);
  body.scrollTop = scroll;
  const wanted = anchor.y - anchor.offset + body.scrollTop;
  outer.scrollTop += body.getBoundingClientRect().top - wanted;
  view.expectedInner = body.scrollTop;
  view.expectedOuter = outer.scrollTop;
  view.top = body.scrollTop;
}
function finishReadingMotion(motion) {
  cancelAnimationFrame(motion.frame);
  handlingReadingMotions.delete(motion.key);
  const { body, el, anchor } = motion;
  if (!body.isConnected) return;
  const view = readingView(el),
    limit = parseFloat(getComputedStyle(body).getPropertyValue('--handling-preview-height'));
  readingAnchorOffset(body, anchor);
  el.classList.toggle('is-expanded', view.expanded);
  el.classList.remove('is-resizing');
  body.style.height = '';
  body.style.maxHeight = '';
  positionReading(
    body,
    anchor,
    view.expanded ? 0 : Math.max(0, anchor.offset - Math.min(limit, anchor.compactInset)),
  );
  // At document edges a browser may clamp the pane's offset. Keep the actual
  // final anchor for a reversible next toggle rather than accumulating drift.
  anchor.y = body.getBoundingClientRect().top + anchor.offset - body.scrollTop;
  view.anchor = anchor;
  view.anchorBody = body;
  handlingReadingToggled(el, view.expanded, true);
  measureHandlingReadings();
}
function toggleHandlingReading(el) {
  const body = el.querySelector('.handling-reading-body'),
    view = readingView(el),
    key = el.dataset.handlingReading,
    previous = handlingReadingMotions.get(key);
  if (previous) cancelAnimationFrame(previous.frame);
  if (!view.expanded && !previous) {
    view.gutter = Math.max(0, body.offsetWidth - body.clientWidth);
    el.style.setProperty('--handling-scrollbar-width', view.gutter + 'px');
  }
  const reusable =
    view.anchorBody === body &&
    view.anchor &&
    (!view.anchor.range || view.anchor.range.startContainer.isConnected);
  const anchor = reusable ? view.anchor : readingAnchor(body);
  readingAnchorOffset(body, anchor);
  const height = body.clientHeight,
    scroll = body.scrollTop;
  const streamView = handlingStreams.get(body);
  if (streamView && !previous) streamView.toggleFollow = handlingStreamAtLatest(body);
  view.expanded = !view.expanded;
  view.anchor = anchor;
  view.anchorBody = body;
  handlingReadingEdges.delete(body);
  const button = el.querySelector('[data-handling-reading-toggle]');
  button.setAttribute('aria-expanded', String(view.expanded));
  button.textContent = view.expanded ? '收起 ↑' : '展开全部 ↓';
  el.classList.add('is-expanded', 'is-resizing');
  body.style.height = height + 'px';
  body.style.maxHeight = 'none';
  const motion = { key, el, body, anchor, height, frame: 0 };
  handlingReadingMotions.set(key, motion);
  handlingReadingToggled(el, view.expanded);
  const start = performance.now();
  const step = (now) => {
    if (!body.isConnected) {
      handlingReadingMotions.delete(key);
      return;
    }
    const full = body.scrollHeight,
      limit = parseFloat(getComputedStyle(body).getPropertyValue('--handling-preview-height')),
      endHeight = view.expanded ? full : Math.min(full, limit);
    const endScroll = view.expanded
      ? 0
      : Math.max(
          0,
          Math.min(anchor.offset - Math.min(endHeight, anchor.compactInset), full - endHeight),
        );
    const t = motionReduced() ? 1 : Math.min(1, (now - start) / motionDuration),
      ease = 1 - (1 - t) ** 3;
    readingAnchorOffset(body, anchor);
    motion.height = height + (endHeight - height) * ease;
    body.style.height = motion.height + 'px';
    positionReading(body, anchor, scroll + (endScroll - scroll) * ease);
    if (t < 1) motion.frame = requestAnimationFrame(step);
    else finishReadingMotion(motion);
  };
  if (motionReduced()) step(start + motionDuration);
  else motion.frame = requestAnimationFrame(step);
}
function invalidateHandlingReading(body) {
  if (body) readingView(body.parentElement).anchor = null;
}
function measureHandlingReadings() {
  for (const node of handlingReadingObserved)
    if (!node.isConnected) {
      handlingReadingSizes.unobserve(node);
      handlingReadingObserved.delete(node);
    }
  for (const view of handlingReadingViews.values())
    if (view.anchorBody && !view.anchorBody.isConnected) {
      view.anchor = null;
      view.anchorBody = null;
    }
  for (const el of document.querySelectorAll('[data-handling-reading]')) {
    const body = el.querySelector('.handling-reading-body'),
      button = el.querySelector('[data-handling-reading-toggle]'),
      key = el.dataset.handlingReading,
      view = handlingReadingViews.get(key),
      motion = handlingReadingMotions.get(key);
    el.classList.toggle('is-resizing', !!motion && motion.body === body);
    for (const node of [body, ...body.children])
      if (!handlingReadingObserved.has(node)) {
        handlingReadingObserved.add(node);
        handlingReadingSizes.observe(node);
      }
    const outer = el.closest('.detail'),
      outerStyle = outer && getComputedStyle(outer);
    el.style.setProperty('--handling-bottom-inset', outerStyle?.paddingBottom || '0px');
    el.style.setProperty('--handling-top-inset', outerStyle?.paddingTop || '0px');
    if (!el.classList.contains('is-expanded'))
      el.style.setProperty(
        '--handling-scrollbar-width',
        Math.max(0, body.offsetWidth - body.clientWidth) + 'px',
      );
    else if (view?.gutter != null)
      el.style.setProperty('--handling-scrollbar-width', view.gutter + 'px');
    if (motion && motion.body !== body) {
      cancelAnimationFrame(motion.frame);
      handlingReadingMotions.delete(key);
      body.style.height = '';
      body.style.maxHeight = '';
      el.classList.remove('is-resizing');
      el.classList.toggle('is-expanded', !!view?.expanded);
    }
    if (
      !handlingReadingNodes.has(body) &&
      (!body.hasAttribute('data-handling-stream') || body.children.length)
    ) {
      handlingReadingNodes.add(body);
      if (!view?.expanded) body.scrollTop = view?.top || 0;
    }
    const long =
      body.scrollHeight >
      parseFloat(getComputedStyle(body).getPropertyValue('--handling-preview-height')) + 1;
    button.hidden = !long;
    el.classList.toggle('is-long', long);
    el.querySelector('.handling-reading-footer').hidden =
      !long && !el.classList.contains('handling-stream');
    const streamView = handlingStreams.get(body);
    if (streamView) handlingStreamLatest(body, streamView);
  }
}
let handlingReadingFrame = 0;
function scheduleHandlingReadingMeasure() {
  if (!handlingReadingFrame)
    handlingReadingFrame = requestAnimationFrame(() => {
      handlingReadingFrame = 0;
      measureHandlingReadings();
      resizeHandlingComposers();
    });
}
new MutationObserver(scheduleHandlingReadingMeasure).observe($('#view'), {
  childList: true,
  subtree: true,
});
window.addEventListener('resize', () => {
  for (const motion of [...handlingReadingMotions.values()]) finishReadingMotion(motion);
  for (const view of handlingReadingViews.values()) view.anchor = null;
  scheduleHandlingReadingMeasure();
});
matchMedia('(prefers-reduced-motion: reduce)').addEventListener('change', (e) => {
  if (e.matches)
    for (const motion of [...handlingReadingMotions.values()]) finishReadingMotion(motion);
});
document.addEventListener('click', (e) => {
  const button = e.target.closest('[data-handling-reading-toggle]');
  if (button) toggleHandlingReading(button.closest('[data-handling-reading]'));
});
document.addEventListener(
  'scroll',
  (e) => {
    if (e.target.matches?.('.handling-reading-body')) {
      const body = e.target,
        view = readingView(body.parentElement);
      if (Math.abs(body.scrollTop - (view.expectedInner ?? -2)) < 1) return;
      view.anchor = null;
      if (!view.expanded) view.top = body.scrollTop;
    }
    if (e.target.matches?.('.detail'))
      for (const el of e.target.querySelectorAll('[data-handling-reading]')) {
        const view = handlingReadingViews.get(el.dataset.handlingReading);
        if (view && Math.abs(e.target.scrollTop - (view.expectedOuter ?? -2)) >= 1)
          view.anchor = null;
      }
  },
  true,
);
// Own compact vertical gestures so a browser's wheel-sequence latch cannot
// require a pause before the outer pane moves. Noncancelable edge events can
// still move the outer pane; CSS containment prevents a second native chain.
document.addEventListener(
  'wheel',
  (event) => {
    if (
      event.defaultPrevented ||
      event.ctrlKey ||
      event.shiftKey ||
      !event.deltaY ||
      Math.abs(event.deltaX) > Math.abs(event.deltaY) ||
      !$('#overlay').hidden
    )
      return;
    const body = event.target.closest?.('.handling-reading-body'),
      outer = body?.closest('.detail');
    if (!outer) return;
    const el = body.parentElement,
      motion = handlingReadingMotions.get(el.dataset.handlingReading);
    if (motion) finishReadingMotion(motion);
    invalidateHandlingReading(body);
    if (readingView(el).expanded) return;
    const direction = Math.sign(event.deltaY),
      room =
        direction < 0
          ? body.scrollTop > 1
          : body.scrollHeight - body.clientHeight - body.scrollTop > 1;
    const unit =
        event.deltaMode === 1
          ? parseFloat(getComputedStyle(body).lineHeight) || 20
          : event.deltaMode === 2
            ? body.clientHeight
            : 1,
      delta = event.deltaY * unit,
      now = performance.now();
    const consume = () => {
      if (event.cancelable) event.preventDefault();
    };
    if (body.scrollHeight <= body.clientHeight + 1) {
      consume();
      handlingReadingEdges.delete(body);
      outer.scrollTop += delta;
      return;
    }
    if (room) {
      handlingReadingEdges.delete(body);
      if (event.cancelable) {
        consume();
        body.scrollTop += delta;
        const arrived =
          direction < 0
            ? body.scrollTop <= 1
            : body.scrollHeight - body.clientHeight - body.scrollTop <= 1;
        if (arrived) handlingReadingEdges.set(body, { direction, since: now });
      }
      return;
    }
    consume();
    let edge = handlingReadingEdges.get(body);
    if (!edge || edge.direction !== direction) {
      edge = { direction, since: now };
      handlingReadingEdges.set(body, edge);
    }
    if (now - edge.since >= handlingReadingEdgeDelay) outer.scrollTop += delta;
  },
  { passive: false, capture: true },
);
