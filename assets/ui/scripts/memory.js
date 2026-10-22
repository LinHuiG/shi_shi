let memoryData = null,
  memoryEditor = null,
  memoryRequest = 0;
async function renderMemory() {
  const request = ++memoryRequest,
    epoch = renderEpoch;
  if (!memoryData)
    setHtml(
      '#view',
      `<div class="page memory-page">${systemPromptTabs()}<div class="box">读取 AI 记忆…</div></div>`,
    );
  const result = await rpc('memory_read');
  if (
    page !== 'prompts' ||
    promptSection !== 'memory' ||
    request !== memoryRequest ||
    epoch !== renderEpoch
  )
    return;
  memoryData = result;
  drawMemory();
}
function memoryTextCount() {
  if (!memoryEditor) return 0;
  return [
    ...(memoryEditor.kind === 'temporary'
      ? memoryEditor.text
      : memoryEditor.entries
          .filter((e) => e.key || e.text)
          .map((e) => `- ${e.key}：${e.text}`)
          .join('\n')),
  ].length;
}
function drawMemory() {
  const edit = memoryEditor,
    kind = edit?.kind;
  const card = (k) => {
    const data = memoryData[k],
      editing = kind === k;
    return `<section class="box memory-card"><div class="row memory-card-head"><div class="memory-heading"><h2>${k === 'temporary' ? '临时记忆' : '归档记忆'}</h2><span class="memory-caption" title="${k === 'temporary' ? '保留本轮分析笔记，供下一轮参考' : '从操作记录中积累的长期经验和偏好'}">${k === 'temporary' ? '保留本轮分析笔记，供下一轮参考' : '从操作记录中积累的长期经验和偏好'}</span></div>${!editing ? `<button data-memory-edit="${k}">编辑记忆</button>` : ''}</div>${editing ? `<div class="memory-editor">${k === 'temporary' ? `<label for="memory-temporary">记忆正文</label><textarea id="memory-temporary" rows="9" data-memory-text>${esc(edit.text)}</textarea>` : `<div id="memory-entries">${edit.entries.map((e, i) => `<div class="memory-entry" data-memory-index="${i}"><label>主题<input data-memory-key value="${esc(e.key)}" maxlength="80"></label><label>记忆内容<textarea data-memory-body rows="3">${esc(e.text)}</textarea></label><button class="quiet" data-memory-remove="${i}">删除此条</button></div>`).join('')}</div><button data-memory-add>＋ 添加记忆</button>`}<div class="memory-editor-foot"><span id="memory-count" class="helper">${memoryTextCount()} / ${data.max_chars} 字符</span><div class="row"><button data-memory-cancel>取消</button><button class="primary" data-memory-save>保存记忆</button></div></div><div id="memory-error" class="error-box" role="status"></div></div>` : k === 'temporary' ? `<p class="memory-prose">${esc(data.text || '还没有临时记忆，AI 会在分析时按需记录。')}</p>` : `${(data.entries || []).map((e) => `<article class="memory-record"><h3>${esc(e.key)}</h3><p class="memory-prose">${esc(e.text)}</p></article>`).join('') || '<p class="helper">还没有归档记忆。</p>'}`}</section>`;
  };
  setHtml(
    '#view',
    `<div class="page memory-page">${systemPromptTabs()}<div class="memory-intro"><h2>AI 记忆</h2><p>看看 AI 记住了什么，随时修正，让后续处理更贴近你的需要。</p><p class="helper">记忆只保存在本地；保存后供后续 AI 请求使用。记忆不提供版本比较或回滚。</p><button class="quiet" data-memory-reload>重新读取</button></div>${card('temporary')}${card('archived')}</div>`,
  );
  $('#view').dataset.contentPage = 'prompts';
  $('#view').dataset.renderedPromptSection = 'memory';
}
function readMemoryDraft() {
  if (!memoryEditor) return;
  if (memoryEditor.kind === 'temporary')
    memoryEditor.text = $('#memory-temporary')?.value ?? memoryEditor.text;
  else
    memoryEditor.entries = [...document.querySelectorAll('[data-memory-index]')].map((el) => ({
      ...memoryEditor.entries[Number(el.dataset.memoryIndex)],
      key: el.querySelector('[data-memory-key]').value,
      text: el.querySelector('[data-memory-body]').value,
    }));
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
  const count = memoryTextCount();
  $('#memory-count').textContent = `${count} / ${memoryData[memoryEditor.kind].max_chars} 字符`;
  $('#memory-count').classList.toggle(
    'memory-over-limit',
    count > memoryData[memoryEditor.kind].max_chars,
  );
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
        entries: structuredClone(data.entries || []),
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
    if (b.hasAttribute('data-memory-add')) {
      memoryEditor.entries.push({ id: '', key: '', text: '' });
      drawMemory();
      return;
    }
    if (b.hasAttribute('data-memory-remove')) {
      memoryEditor.entries.splice(Number(b.dataset.memoryRemove), 1);
      drawMemory();
      return;
    }
    if (b.hasAttribute('data-memory-save')) {
      if (memoryEditor.saving) return;
      memoryEditor.saving = true;
      document
        .querySelectorAll('.memory-page button,.memory-page input,.memory-page textarea')
        .forEach((el) => (el.disabled = true));
      try {
        const result = await rpc('memory_save', memoryEditor);
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
