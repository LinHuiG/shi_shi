const handlingStreams = new WeakMap();
function handlingLivePanel(r) {
  return handlingReadingPanel(
    'stream',
    handlingReadingKey('stream', r, r.rounds?.at(-1)),
    '',
    '',
    `data-handling-stream="${esc(r.id)}" data-preserve-content aria-label="处置实时进度"`,
  );
}
const handlingStreamExpanded = (output) =>
  output.closest('[data-handling-reading]').classList.contains('is-expanded');
function handlingStreamTarget(output) {
  const expanded = handlingStreamExpanded(output),
    scroller = expanded ? output.closest('.detail') : output;
  const footer = output.parentElement.querySelector('.handling-reading-footer');
  const style = expanded ? getComputedStyle(scroller) : null;
  // The sticky footer offsets the pane's bottom padding and sits at its actual
  // visible edge. Reserve its entire height when bringing the tail into view.
  const visibleBottom = expanded
    ? scroller.getBoundingClientRect().bottom - parseFloat(style.borderBottomWidth)
    : 0;
  const end = expanded
    ? scroller.scrollTop +
      output.getBoundingClientRect().bottom +
      footer.offsetHeight -
      visibleBottom +
      12
    : output.scrollHeight - output.clientHeight;
  return {
    scroller,
    end: Math.max(0, Math.min(end, scroller.scrollHeight - scroller.clientHeight)),
  };
}
function handlingStreamAtLatest(output) {
  const { scroller, end } = handlingStreamTarget(output);
  return end - scroller.scrollTop < 24;
}
function handlingStreamLatest(output, view) {
  output.parentElement.querySelector('[data-handling-latest]').hidden =
    view.follow || handlingStreamAtLatest(output);
}
function pauseHandlingStream(output, view) {
  view.follow = false;
  cancelAnimationFrame(view.frame);
  view.frame = 0;
  view.expected = -1;
  handlingStreamLatest(output, view);
}
function followHandlingStream(output, view) {
  if (
    !view.follow ||
    view.frame ||
    handlingReadingMotions.has(output.parentElement.dataset.handlingReading)
  )
    return;
  const start = performance.now(),
    top = handlingStreamTarget(output).scroller.scrollTop;
  const step = (now) => {
    view.frame = 0;
    if (!output.isConnected || !view.follow) return;
    const { scroller, end } = handlingStreamTarget(output),
      t = motionReduced() ? 1 : Math.min(1, (now - start) / motionDuration);
    view.expected = top + (end - top) * (1 - (1 - t) ** 3);
    scroller.scrollTop = view.expected;
    view.expected = scroller.scrollTop;
    handlingStreamLatest(output, view);
    if (t < 1 && Math.abs(end - scroller.scrollTop) > 1) view.frame = requestAnimationFrame(step);
  };
  view.frame = requestAnimationFrame(step);
}
function handlingReadingToggled(el, expanded, settled = false) {
  const output = el.querySelector('[data-handling-stream]'),
    view = output && handlingStreams.get(output);
  if (!view) return;
  // Expansion preserves reading position. Either view resumes following only
  // when its actual tail is visible; explicit latest works in either view.
  pauseHandlingStream(output, view);
  view.follow = settled && (view.toggleFollow || handlingStreamAtLatest(output));
  if (settled) delete view.toggleFollow;
  view.expected = handlingStreamTarget(output).scroller.scrollTop;
  handlingStreamLatest(output, view);
  followHandlingStream(output, view);
}
function updateHandlingLive() {
  for (const output of document.querySelectorAll('[data-handling-stream]')) {
    const run = (state?.handling || []).find((r) => r.id === output.dataset.handlingStream);
    if (!run) continue;
    let view = handlingStreams.get(output);
    const initial = !view,
      saved = handlingReadingViews.get(output.parentElement.dataset.handlingReading);
    if (!view) {
      view = {
        follow: !handlingStreamExpanded(output) && saved?.top == null,
        frame: 0,
        expected: -1,
        round: '',
      };
      handlingStreams.set(output, view);
    }
    const round = run.rounds?.at(-1),
      roundKey = round?.id || round?.turn_id || run.id;
    if (view.round !== roundKey) {
      output.replaceChildren();
      view.round = roundKey;
      view.follow = !handlingStreamExpanded(output) && saved?.top == null;
      cancelAnimationFrame(view.frame);
      view.frame = 0;
    }
    const steps = (round?.steps || []).filter((step) => step.text),
      entries = steps.length
        ? steps
        : run.progress
          ? [{ id: 'progress', kind: 'agentMessage', text: run.progress }]
          : [];
    for (const [index, entry] of entries.entries()) {
      const key = String(entry.id ?? index);
      let el = [...output.children].find((el) => el.dataset.streamEntry === key);
      const detail = entry.kind !== 'agentMessage';
      if (el && el.dataset.streamKind !== entry.kind) {
        el.remove();
        el = null;
      }
      if (!el) {
        el = document.createElement(detail ? 'details' : 'section');
        el.className = 'handling-stream-entry' + (detail ? ' handling-stream-detail' : '');
        el.dataset.streamEntry = key;
        el.dataset.streamKind = entry.kind;
        const title = document.createElement(detail ? 'summary' : 'h4'),
          body = document.createElement('div');
        title.textContent =
          {
            agentMessage: '进展说明',
            commandExecution: '执行命令',
            fileChange: '文件变更',
            mcpToolCall: '工具进度',
            webSearch: '网页检索',
            userMessage: '输入消息',
            functionCall: '执行工具',
            functionCallOutput: '工具输出',
          }[entry.kind] || '处置进展';
        body.className = 'handling-stream-text';
        el.append(title, body);
        output.append(el);
      }
      const body = el.querySelector('.handling-stream-text'),
        text = String(entry.text || '');
      // A shorter calibration snapshot must not rewind an ongoing visible item.
      const target = smoothTexts.get(body)?.target || body.textContent;
      if (handlingActive(run) && target.startsWith(text) && target.length > text.length) continue;
      smoothText(body, text, () => followHandlingStream(output, view), initial);
    }
    motionText(
      output.parentElement.querySelector('[data-handling-stream-status]'),
      handlingActive(run) ? '持续同步进展' : '本轮进度',
    );
    handlingStreamLatest(output, view);
    followHandlingStream(output, view);
  }
  measureHandlingReadings();
}
document.addEventListener(
  'scroll',
  (e) => {
    if (e.target.matches?.('[data-handling-stream]')) {
      const output = e.target,
        view = handlingStreams.get(output);
      if (!view || handlingStreamExpanded(output) || Math.abs(output.scrollTop - view.expected) < 2)
        return;
      view.follow = output.scrollHeight - output.clientHeight - output.scrollTop < 24;
      if (!view.follow) pauseHandlingStream(output, view);
      else handlingStreamLatest(output, view);
      return;
    }
    if (!e.target.matches?.('.detail')) return;
    for (const output of e.target.querySelectorAll('[data-handling-stream]')) {
      const view = handlingStreams.get(output);
      if (
        !view ||
        !handlingStreamExpanded(output) ||
        Math.abs(e.target.scrollTop - view.expected) < 2
      )
        continue;
      pauseHandlingStream(output, view);
      if (handlingStreamAtLatest(output)) view.follow = true;
      handlingStreamLatest(output, view);
    }
  },
  true,
);
document.addEventListener(
  'wheel',
  (e) => {
    const outer = e.target.closest?.('.detail');
    if (!outer || !$('#overlay').hidden || !e.deltaY) return;
    for (const output of outer.querySelectorAll('[data-handling-stream]')) {
      const view = handlingStreams.get(output);
      if (view && (handlingStreamExpanded(output) || (output.contains(e.target) && e.deltaY < 0)))
        pauseHandlingStream(output, view);
    }
  },
  { passive: true },
);
document.addEventListener('click', (e) => {
  const button = e.target.closest('[data-handling-latest]');
  if (!button) return;
  const output = button.closest('.handling-stream').querySelector('[data-handling-stream]'),
    view = handlingStreams.get(output);
  if (view) {
    invalidateHandlingReading(output);
    view.follow = true;
    followHandlingStream(output, view);
    button.hidden = true;
  }
});
document.addEventListener(
  'toggle',
  (e) => {
    if (!e.target.matches?.('.handling-stream-detail')) return;
    const output = e.target.closest('[data-handling-stream]'),
      view = output && handlingStreams.get(output);
    if (view) {
      invalidateHandlingReading(output);
      pauseHandlingStream(output, view);
    }
    scheduleHandlingReadingMeasure();
  },
  true,
);
