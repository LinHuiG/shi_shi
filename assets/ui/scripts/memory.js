let memoryData = null,
  memoryEditor = null,
  memoryRequest = 0;
async function renderMemory() {
  const request = ++memoryRequest,
    epoch = renderEpoch;
  if (!memoryData)
    setHtml('#settings-prompt-content', `<div class="memory-page">${systemPromptTabs()}<p class="helper">读取AI记忆…</p></div>`);
  const result = await rpc('memory_read');
  if (page !== 'settings' || promptSection !== 'memory' || request !== memoryRequest || epoch !== renderEpoch) return;
  memoryData = result;
  drawMemory();
}
function memoryTextCount() {
  return [...(memoryEditor?.text || '')].length;
}
function drawMemory() {
  const edit = memoryEditor,
    kind = edit?.kind;
  const card = (k) => {
    const data = memoryData[k],
      editing = kind === k,
      caption = k === 'temporary' ? '分析之间的交接笔记，归档时并入归档记忆后清空' : '归档时结合操作记录和临时记忆，整理长期经验和偏好',
      text = editing ? edit.text : data.text || '',
      count = [...text].length,
      countLabel = `${count} / ${data.max_chars} 字符`,
      overLimit = count > data.max_chars;
    return `<section class="box memory-card"><div class="row memory-card-head"><div class="memory-heading"><h2>${k === 'temporary' ? '临时记忆' : '归档记忆'}</h2><span class="memory-caption" title="${caption}">${caption}</span></div>${!editing ? `<button data-memory-edit="${k}">编辑记忆</button>` : ''}</div>${editing ? `<div class="memory-editor"><label for="memory-${k}">记忆正文</label><textarea id="memory-${k}" rows="9" data-memory-text>${esc(text)}</textarea><div class="memory-editor-foot"><span id="memory-count" class="helper${overLimit ? ' memory-over-limit' : ''}">${countLabel}</span><div class="row"><button data-memory-cancel>取消</button><button class="primary" data-memory-save>保存记忆</button></div></div><p id="memory-limit" class="helper memory-over-limit"${overLimit ? '' : ' hidden'}>原内容已完整保留，请精简到 ${data.max_chars} 字符以内再保存。</p><div id="memory-error" class="error-box" role="status"></div></div>` : `<p class="memory-prose">${esc(text || (k === 'temporary' ? '还没有临时记忆，AI 会在分析时按需记录。' : '还没有归档记忆。'))}</p><p class="helper${overLimit ? ' memory-over-limit' : ''}">${countLabel}${overLimit ? ' · 原内容已完整保留，后续保存需精简。' : ''}</p>`}</section>`;
  };
  setHtml(
    '#settings-prompt-content',
    `<div class="memory-page">${systemPromptTabs()}<div class="memory-intro"><h2>AI 记忆</h2><p>看看 AI 记住了什么，随时修正，让后续处理更贴近你的需要。</p><p class="helper">记忆只保存在本地；保存后供后续 AI 请求使用。记忆不提供版本比较或回滚。</p><button class="quiet" data-memory-reload>重新读取</button></div>${card('temporary')}${card('archived')}</div>`,
  );
}
function readMemoryDraft() {
  if (!memoryEditor) return;
  memoryEditor.text = $('[data-memory-text]')?.value ?? memoryEditor.text;
}
function leaveMemory() {
  if (!memoryEditor) return true;
  if (memoryEditor.saving) {
    toast('记忆正在保存，请稍候');
    return false;
  }
  readMemoryDraft();
  if (!confirm('记忆修改尚未保存，放弃本次编辑？')) return false;
  memoryEditor = null;
  return true;
}
document.addEventListener('input', (e) => {
  if (!e.target.closest('.memory-editor')) return;
  readMemoryDraft();
  const count = memoryTextCount(),
    limit = memoryData[memoryEditor.kind].max_chars;
  $('#memory-count').textContent = `${count} / ${limit} 字符`;
  $('#memory-count').classList.toggle('memory-over-limit', count > limit);
  $('#memory-limit').hidden = count <= limit;
});
document.addEventListener('click', async (e) => {
  const b = e.target.closest('button');
  if (!b || ![...b.attributes].some((a) => a.name.startsWith('data-memory-'))) return;
  try {
    if (b.hasAttribute('data-memory-edit')) {
      if (!leaveMemory()) return;
      const kind = b.dataset.memoryEdit,
        data = memoryData[kind];
      memoryEditor = {
        kind,
        expected: data.revision,
        text: data.text || '',
      };
      drawMemory();
      return;
    }
    if (b.hasAttribute('data-memory-cancel')) {
      memoryEditor = null;
      drawMemory();
      return;
    }
    if (b.hasAttribute('data-memory-reload')) {
      if (!leaveMemory()) return;
      await renderMemory();
      return;
    }
    readMemoryDraft();
    if (b.hasAttribute('data-memory-save')) {
      if (memoryEditor.saving) return;
      const limit = memoryData[memoryEditor.kind].max_chars;
      if (memoryTextCount() > limit) return;
      memoryEditor.saving = true;
      document
        .querySelectorAll('.memory-page button,.memory-page input,.memory-page textarea')
        .forEach((el) => (el.disabled = true));
      try {
        const { kind, expected, text } = memoryEditor,
          result = await rpc('memory_save', { kind, expected, text });
        memoryData = result;
        memoryEditor = null;
        drawMemory();
        toast('记忆已保存');
      } catch (error) {
        $('#memory-error').textContent = error.message;
      } finally {
        if (memoryEditor) memoryEditor.saving = false;
        document
          .querySelectorAll('.memory-page button,.memory-page input,.memory-page textarea')
          .forEach((el) => (el.disabled = false));
      }
    }
  } catch (error) {
    toast(error.message);
  }
});
