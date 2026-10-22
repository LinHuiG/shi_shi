// Request identity comes from captured logs/state, never from current settings.
document.addEventListener('click', async (e) => {
  const b = e.target.closest('button');
  if (!b) return;
  if (b.dataset.analysisSession) {
    if (b.dataset.sessionDriver === 'dsh') {
      setModal(
        `<h2>DSH 分析会话</h2><p>请在 DeepSeek Harness 客户端中按会话编号查找。本次会话由 DSH 创建。</p><label>会话编号<input readonly value="${esc(b.dataset.analysisSession)}"></label><div class="dialog-foot"><button data-action="close">关闭</button></div>`,
      );
      return;
    }
    try {
      await rpc('analysis_session_open', {
        driver: b.dataset.sessionDriver,
        thread_id: b.dataset.analysisSession,
      });
    } catch (error) {
      toast(error.message);
    }
    return;
  }
  if (b.dataset.traceCompare) {
    b.disabled = true;
    try {
      const d = await rpc('ai_trace', { id: b.dataset.traceCompare, previous: true });
      if (!b.isConnected) return;
      setModal(
        `<h2>请求内容有变化</h2><p class="helper">比较本次请求与前一份已保留的请求。文本变化不表示业务事实冲突，也不代表服务端缓存是否命中。左侧为上次，右侧为本次。</p><div class="trace-comparison">${compareTraceInputs(d.previous_input, d.input)}</div><div class="dialog-foot"><button data-action="close">关闭</button></div>`,
      );
      $('#dialog').classList.add('wide-dialog');
      modalFromTrace = true;
    } catch (error) {
      toast(error.message);
    } finally {
      b.disabled = false;
    }
  }
});
function traceSections(text) {
  const out = new Map();
  let title = '请求头';
  out.set(title, '');
  for (const line of text.split('\n')) {
    if (/^## /.test(line)) {
      title = line.slice(3).replace('（最后）', '');
      if (!out.has(title)) out.set(title, '');
    } else out.set(title, out.get(title) + line + '\n');
  }
  return out;
}
function traceFields(text) {
  const result = new Map();
  let count = 0;
  function walk(value, path) {
    if (++count > 3000) return;
    if (value && typeof value === 'object') {
      for (const [key, v] of Object.entries(value))
        walk(v, path + (Array.isArray(value) ? `[${key}]` : `.${key}`));
    } else result.set(path, JSON.stringify(value));
  }
  const lines = text.trim().split('\n');
  for (let i = 0; i < lines.length; i++) {
    try {
      const value = JSON.parse(lines[i]);
      if (value && typeof value === 'object') walk(value, `数据${i + 1}`);
      else result.set(`文本${i + 1}`, lines[i]);
    } catch {
      result.set(`文本${i + 1}`, lines[i]);
    }
  }
  return { result, truncated: count > 3000 };
}
function compareTraceInputs(previous, current) {
  const a = traceSections(previous),
    b = traceSections(current);
  return [...new Set([...a.keys(), ...b.keys()])]
    .map((title) => {
      const left = a.get(title) || '',
        right = b.get(title) || '';
      if (left === right)
        return `<details><summary>${esc(title)} · 内容相同</summary><pre>${esc(right)}</pre></details>`;
      const x = traceFields(left),
        y = traceFields(right),
        keys = [...new Set([...x.result.keys(), ...y.result.keys()])].filter(
          (k) => x.result.get(k) !== y.result.get(k),
        ),
        rows = keys.slice(0, 200);
      return `<details open><summary>${esc(title)} · ${keys.length} 个字段／文本位置有变化</summary><div class="trace-field-table">${rows.map((k) => `<div><b>${esc(k)}</b><pre>${esc((x.result.get(k) ?? '（无）').slice(0, 2000))}</pre><pre>${esc((y.result.get(k) ?? '（无）').slice(0, 2000))}</pre></div>`).join('')}</div>${keys.length > 200 || x.truncated || y.truncated ? '<p class="helper">字段摘要有界，完整内容见下方。</p>' : ''}<details><summary>完整原文</summary><div class="cols"><pre>${esc(left)}</pre><pre>${esc(right)}</pre></div></details></details>`;
    })
    .join('');
}

const smoothTexts = new WeakMap();
function smoothText(node, text, changed, immediate = false) {
  if (!node) return;
  let a = smoothTexts.get(node);
  if (!a) {
    a = { target: '', frame: 0, deadline: 0 };
    smoothTexts.set(node, a);
  }
  if (a.target === text && node.textContent === text) return;
  a.target = text;
  if (immediate || motionReduced() || !text.startsWith(node.textContent)) {
    cancelAnimationFrame(a.frame);
    a.frame = 0;
    motionText(node, text, true);
    changed?.();
    return;
  }
  if (a.frame) return;
  a.deadline = performance.now() + motionDuration;
  let last = performance.now();
  const step = (now) => {
    a.frame = 0;
    if (!node.isConnected) return;
    const current = node.textContent,
      target = a.target;
    if (!target.startsWith(current)) {
      motionText(node, target);
      changed?.();
      return;
    }
    const rest = Array.from(target.slice(current.length)),
      amount =
        now >= a.deadline
          ? rest.length
          : Math.max(1, Math.ceil((rest.length * (now - last)) / Math.max(1, a.deadline - last)));
    last = now;
    const addition = rest.slice(0, amount).join('');
    if (node.lastChild?.nodeType === Node.TEXT_NODE) node.lastChild.appendData(addition);
    else node.append(document.createTextNode(addition));
    changed?.();
    if (node.textContent !== a.target) a.frame = requestAnimationFrame(step);
  };
  a.frame = requestAnimationFrame(step);
}
let statusStructure = '';
function updateStatusBar(html, phase) {
  const bar = $('#status');
  if (statusStructure !== html || !bar.children.length) {
    setHtml(bar, html);
    statusStructure = html;
  }
  const host = bar.querySelector('.phase-label'),
    spinning = phase.includes('class="spin"');
  if (host) {
    if (!host.querySelector('.phase-copy'))
      host.innerHTML = '<span class="spin" hidden></span><span class="phase-copy"></span>';
    host.querySelector('.spin').hidden = !spinning;
    motionText(host.querySelector('.phase-copy'), phase.replace(/<[^>]*>/g, ''));
  }
  const speed = bar.querySelector('[data-status-speed]');
  if (speed) {
    const rate = requestSpeed(
      state.ai_busy ? state.ai_request_id || state.ai_live?.request_id || '' : '',
    );
    speed.hidden = !rate.known;
    speed.textContent = rate.known ? rate.text : '';
    speed.title = rate.tip;
  }
  const line = bar.querySelector('[data-live-line]');
  if (line) {
    const text = analysisProgressText();
    smoothText(line, text, undefined, !smoothTexts.has(line));
    line.parentElement.title = text;
  }
}
let liveDetailRequest = '',
  liveFollow = true,
  liveExpectedScroll = -1,
  liveScrollFrame = 0,
  liveOpening = false;
function followLiveOutput() {
  const output = $('#ai-live-output');
  if (!output || !liveFollow || liveScrollFrame) return;
  const start = performance.now(),
    top = output.scrollTop;
  const step = (now) => {
    liveScrollFrame = 0;
    if (!output.isConnected || !liveFollow) return;
    const end = output.scrollHeight - output.clientHeight,
      t = motionReduced() ? 1 : Math.min(1, (now - start) / motionDuration);
    liveExpectedScroll = top + (end - top) * (1 - (1 - t) ** 3);
    output.scrollTop = liveExpectedScroll;
    liveExpectedScroll = output.scrollTop;
    if (t < 1 && end - output.scrollTop > 1) liveScrollFrame = requestAnimationFrame(step);
  };
  liveScrollFrame = requestAnimationFrame(step);
}
function updateAiLiveDetail() {
  const output = $('#ai-live-output');
  if (!output) return;
  const live = state?.ai_live,
    metrics = requestMetrics(liveDetailRequest);
  const speed = $('#ai-live-speed'),
    latency = $('#ai-live-latency');
  if (speed) {
    const rate = requestSpeed(liveDetailRequest);
    speed.parentElement.hidden = !rate.known;
    speed.textContent = rate.known ? rate.text : '';
    speed.title = rate.tip;
    $('#ai-live-speed-label').textContent = rate.average ? '平均速度' : '速度';
  }
  if (latency) {
    latency.parentElement.hidden = !Number.isFinite(metrics.first);
    latency.textContent = Number.isFinite(metrics.first) ? firstResponseLabel(metrics.first) : '';
  }
  if (live?.request_id !== liveDetailRequest) {
    $('#ai-live-detail-status').textContent = '本次请求已结束';
    return;
  }
  $('#ai-live-detail-status').textContent = state.ai_busy
    ? live.phase || '正在处理'
    : '本次处理已结束';
  if (!liveScrollFrame)
    liveFollow = output.scrollHeight - output.clientHeight - output.scrollTop < 36;
  const labels = {
      reasoning: '思考进展',
      content: '正在生成',
      tool: '工具进度',
      phase: '当前阶段',
    },
    entries = live.entries || [],
    keep = new Set();
  entries.forEach((entry, index) => {
    const key = String(entry.id ?? index);
    keep.add(key);
    let el = [...output.children].find((el) => el.dataset.liveEntry === key);
    if (!el) {
      el = document.createElement('section');
      el.className = 'live-entry';
      el.dataset.liveEntry = key;
      const h = document.createElement('h3'),
        body = document.createElement('div');
      h.textContent = labels[entry.kind] || '进度';
      body.className = 'live-entry-text';
      el.append(h, body);
      output.append(el);
    }
    const body = el.querySelector('.live-entry-text');
    smoothText(body, entry.text, followLiveOutput, liveOpening);
  });
  for (const el of [...output.children]) if (!keep.has(el.dataset.liveEntry)) el.remove();
  if (!entries.length) output.dataset.empty = '等待 AI 返回公开进展…';
  else delete output.dataset.empty;
  $('#ai-live-latest').hidden = liveFollow;
  followLiveOutput();
  liveOpening = false;
}
document.addEventListener(
  'scroll',
  (e) => {
    const el = e.target;
    if (el.id !== 'ai-live-output') return;
    if (Math.abs(el.scrollTop - liveExpectedScroll) < 2) return;
    liveFollow = el.scrollHeight - el.clientHeight - el.scrollTop < 36;
    if (!liveFollow) {
      cancelAnimationFrame(liveScrollFrame);
      liveScrollFrame = 0;
    }
    $('#ai-live-latest').hidden = liveFollow;
  },
  true,
);
document.addEventListener(
  'wheel',
  (e) => {
    if (e.target.closest('#ai-live-output') && e.deltaY < 0) {
      liveFollow = false;
      cancelAnimationFrame(liveScrollFrame);
      liveScrollFrame = 0;
      liveExpectedScroll = -1;
    }
  },
  { passive: true },
);
document.addEventListener('click', (e) => {
  if (e.target.closest('#ai-live-latest')) {
    liveFollow = true;
    followLiveOutput();
    $('#ai-live-latest').hidden = true;
    return;
  }
  if (!e.target.closest('[data-ai-live]')) return;
  liveDetailRequest = state.ai_live?.request_id || '';
  liveFollow = true;
  liveExpectedScroll = -1;
  liveOpening = true;
  cancelAnimationFrame(liveScrollFrame);
  liveScrollFrame = 0;
  setModal(
    '<div class="live-heading"><span class="live-mark" aria-hidden="true">✧</span><div><h2>AI 正在做什么</h2><p id="ai-live-detail-status" class="helper"></p></div></div><div id="ai-live-output" class="ai-live-output"></div><div class="live-reading-bar"><div class="live-metrics"><div title="从本次请求发起到首次收到 AI 公开思考或正文的耗时；阶段和工具消息不计入。"><span>首字节延迟</span><b id="ai-live-latency">—</b></div><div hidden><span id="ai-live-speed-label">速度</span><b id="ai-live-speed"></b></div></div><button id="ai-live-latest" class="quiet" hidden>回到最新 ↓</button></div><div class="dialog-foot"><button data-action="close">关闭</button></div>',
  );
  $('#dialog').classList.add('live-dialog');
  updateAiLiveDetail();
});
