document.addEventListener('click', async (e) => {
  const b = e.target.closest('button');
  if (!b) return;
  try {
    if (b.dataset.imageId) {
      b.disabled = true;
      try {
        const image = await rpc('image_preview', { image_id: b.dataset.imageId });
        const img = document.createElement('img');
        img.src = image.url;
        img.alt = '消息图片';
        img.style.cssText =
          'max-width:100%;max-height:360px;object-fit:contain;display:block;border-radius:8px;margin:8px 0';
        b.parentElement.append(img);
        b.textContent = '已加载本地图片';
      } catch (error) {
        b.textContent = error.message;
      }
      return;
    }
    if (!b.closest('#backlog-panel') && b.dataset.action !== 'backlog') closeBacklog();
    if (b.dataset.scheduleEdit) {
      scheduleForm(state.schedules.find((s) => s.id === b.dataset.scheduleEdit));
      return;
    }
    if (b.dataset.scheduleToggle) {
      const s = state.schedules.find((s) => s.id === b.dataset.scheduleToggle);
      b.disabled = true;
      try {
        await perform(
          'schedule_edit',
          { schedule: { ...s, enabled: !s.enabled } },
          s.enabled ? '已暂停定时事项' : '已启用定时事项',
        );
      } finally {
        b.disabled = false;
      }
      return;
    }
    if (b.dataset.scheduleRemove) {
      const s = state.schedules.find((s) => s.id === b.dataset.scheduleRemove);
      modalSchedule = structuredClone(s);
      setModal(
        `<h2>删除定时事项</h2><p>${esc(s.title)}</p><p class="helper">停止后续生成，已经生成的事项会保留。</p><div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">取消</button><button data-action="delete-schedule">删除规则</button></div>`,
      );
      return;
    }
    if ((b.dataset.recent || b.dataset.view) && !(await leaveSettings())) return;
    if (b.dataset.recent) {
      page = 'tasks';
      tab = '全部';
      quickView = '';
      selected = b.dataset.recent;
      render();
      return;
    }
    if (b.dataset.view) {
      page = 'tasks';
      tab = b.dataset.view === 'daily' ? '今日事项' : '待处理';
      quickView = b.dataset.view === 'daily' ? '' : b.dataset.view;
      render();
      return;
    }
    if (b.dataset.logFilter) {
      logFilter = b.dataset.logFilter;
      logPage = 1;
      void renderLogs();
      return;
    }
    if (b.dataset.page) {
      if (!(await leaveSettings())) return;
      page = b.dataset.page;
      render();
      return;
    }
    if (b.dataset.tab) {
      tab = tab === b.dataset.tab ? '全部' : b.dataset.tab;
      quickView = '';
      renderTasks();
      return;
    }
    if (b.dataset.task) {
      selected = b.dataset.task;
      renderTasks();
      return;
    }
    if (b.dataset.build) {
      const m =
        inboxRows().find((m) => m.id === b.dataset.build) ||
        state.messages.find((m) => m.id === b.dataset.build);
      newModal(readableMessage(m?.text || '').text, [b.dataset.build]);
      return;
    }
    if (b.dataset.rule) {
      await perform('rule', { id: b.dataset.rule }, '规则已更新，保留版本记录');
      return;
    }
    if (b.dataset.undo) {
      await perform('undo', { index: Number(b.dataset.undo) }, '已撤销');
      return;
    }
    if (b.dataset.resetPrompt) {
      if ($('#prompt-content')) {
        $('#prompt-content').value = promptDefaults[b.dataset.resetPrompt];
        settingsDirty = true;
      }
      return;
    }
    if (b.dataset.trace) {
      const d = await rpc('ai_trace', { id: b.dataset.trace });
      setModal(
        `<h2>AI 调用详情</h2><div class="helper">${esc(b.dataset.trace)} · 本地保存的实际输入与返回</div><div class="trace-content"><h3>实际提示词与输入数据</h3><pre>${esc(d.input || '未记录')}</pre><h3>模型输出</h3><pre>${esc(d.output || '未保存最终输出详情；可查看下方已接收事件或调用状态')}</pre>${d.error ? `<div class="error-box">${esc(d.error)}</div>` : ''}<details><summary>输出格式约束</summary><pre>${esc(d.schema)}</pre></details><details><summary>原始响应 / 已接收事件</summary><pre>${esc(d.events || '暂无事件')}</pre></details></div><div class="dialog-foot"><button data-action="close">关闭</button></div>`,
      );
      $('#dialog .dialog-foot').insertAdjacentHTML(
        'afterbegin',
        `<button data-trace-compare="${esc(b.dataset.trace)}">与上次已保留请求比较</button>`,
      );
      modalFromTrace = true;
      return;
    }
    const action = b.dataset.action;
    const t = selectedTask();
    switch (action) {
      case 'ai-edit-task': {
        const text = $('#edit-instruction').value.trim();
        if (!text) {
          $('#modal-error').textContent = '请描述这条事项需要怎么修改';
          break;
        }
        const task = readTaskForm(),
          controls = [...$('#dialog').querySelectorAll('input,select,textarea,button')];
        controls.forEach((el) => (el.disabled = true));
        b.dataset.pending = 'true';
        const label = b.textContent;
        b.textContent = '正在分析并更新…';
        $('#modal-error').textContent = '';
        try {
          const saved = await rpc('edit_ai', { task, text });
          delete b.dataset.pending;
          closeModal();
          page = 'tasks';
          selected = saved.id;
          await refreshState(true);
          toast('已根据你的说明更新当前事项');
        } catch (err) {
          $('#modal-error').textContent = err.message;
        } finally {
          delete b.dataset.pending;
          controls.forEach((el) => (el.disabled = false));
          b.textContent = label;
        }
        break;
      }

      case 'new-schedule':
        scheduleForm();
        break;
      case 'format-schedule': {
        const text = $('#schedule-natural').value.trim();
        if (!text) {
          $('#modal-error').textContent = '请填写重复频率、时间和要做的事';
          break;
        }
        const draft = readScheduleForm();
        const controls = [...$('#dialog').querySelectorAll('input,select,textarea,button')];
        controls.forEach((el) => (el.disabled = true));
        b.dataset.pending = 'true';
        const label = b.textContent;
        b.textContent = '正在填写…';
        $('#modal-error').textContent = '';
        try {
          const parsed = await rpc('schedule_format', { text });
          scheduleForm(
            {
              ...draft,
              ...parsed,
              id: draft.id || '',
              created: draft.created || 0,
              updated: draft.updated || 0,
              enabled: draft.enabled,
            },
            text,
          );
        } catch (err) {
          $('#modal-error').textContent = err.message;
        } finally {
          delete b.dataset.pending;
          controls.forEach((el) => (el.disabled = false));
          b.textContent = label;
        }
        break;
      }
      case 'save-schedule': {
        const schedule = readScheduleForm();
        b.disabled = true;
        b.dataset.pending = 'true';
        try {
          await rpc('schedule_edit', { schedule });
          delete b.dataset.pending;
          closeModal();
          page = 'schedules';
          await refreshState(true);
          toast('定时事项已保存');
        } catch (err) {
          $('#modal-error').textContent = err.message;
        } finally {
          delete b.dataset.pending;
          b.disabled = false;
        }
        break;
      }
      case 'delete-schedule': {
        b.disabled = true;
        b.dataset.pending = 'true';
        try {
          await rpc('schedule_delete', { id: modalSchedule.id, updated: modalSchedule.updated });
          delete b.dataset.pending;
          closeModal();
          await refreshState(true);
          toast('已删除定时规则，已生成事项保留');
        } catch (err) {
          $('#modal-error').textContent = err.message;
        } finally {
          delete b.dataset.pending;
          b.disabled = false;
        }
        break;
      }
      case 'tomorrow':
      case 'resume-today': {
        b.disabled = true;
        try {
          await perform(
            'action',
            { id: t.id, action: action === 'tomorrow' ? 'tomorrow' : 'resume_today' },
            action === 'tomorrow'
              ? '已移至明日，明天自动回到今日事项；可在“全部”中提前恢复'
              : '已恢复今日处理',
          );
        } finally {
          b.disabled = false;
        }
        break;
      }

      case 'archive-note': {
        setModal(
          `<h2>${t.archived ? '归档备注' : '归档事项'}</h2><p class="subtitle">${esc(t.title)}</p><div class="form-group"><label>归档结果</label><select id="archive-status">${!['已完成', '无需处理', '已取消'].includes(t.status) ? '<option value="">请选择归档结果</option>' : ''}${['已完成', '无需处理', '已取消'].map((s) => `<option ${s === t.status ? 'selected' : ''}>${s}</option>`).join('')}</select></div><div class="form-group"><label>备注（可选）</label><textarea id="archive-note" maxlength="4000" rows="5" placeholder="记录实际处理结果、误判原因，或下次希望如何判断。">${esc(t.archive_note || '')}</textarea></div><div class="helper">此处只保存事项及备注，不调用 AI。晚间或点击“手动归档”后统一分析。</div><div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">取消</button><button class="primary" data-action="save-archive">保存并归档</button></div>`,
        );
        modalTask = structuredClone(t);
        break;
      }
      case 'save-archive': {
        b.disabled = true;
        try {
          await rpc('archive_task', {
            id: modalTask.id,
            updated: modalTask.updated,
            status: $('#archive-status').value,
            note: $('#archive-note').value,
          });
          closeModal();
          await refreshState(true);
          toast('归档已保存，等待晚间或手动归档分析');
        } catch (err) {
          $('#modal-error').textContent = err.message;
          b.disabled = false;
        }
        break;
      }
      case 'toggle-scan': {
        if (scanToggleBusy) break;
        scanToggleBusy = true;
        const enabled = !state.config.enabled;
        b.disabled = true;
        try {
          const result = await rpc('scan_control', { enabled });
          state.config.enabled = result.enabled;
          const input = $('#cfg-enabled');
          if (input) {
            input.checked = result.enabled;
            settingsBaseline['cfg-enabled'] = result.enabled;
            syncAutomationControls();
          }
          status();
          toast(result.enabled ? '已开启增量消息扫描' : '已暂停增量扫描，报表与归档继续按计划运行');
        } finally {
          scanToggleBusy = false;
          b.disabled = false;
        }
        break;
      }
      case 'scan-options': {
        if (page !== 'settings') {
          page = 'settings';
          render();
        }
        document
          .querySelector('#settings-automation')
          ?.scrollIntoView({ behavior: 'smooth', block: 'start' });
        break;
      }
      case 'close':
        closeModal();
        break;
      case 'new':
        newModal();
        break;
      case 'edit':
        formModal(t);
        break;
      case 'manual-form': {
        const text = $('#natural-input').value;
        formModal({ ...blankTask(), description: text });
        break;
      }
      case 'format-new': {
        const text = $('#natural-input').value.trim();
        if (!text) {
          $('#modal-error').textContent = '请输入事项描述';
          break;
        }
        b.disabled = true;
        try {
          const draft = await rpc('generate', { text, sources: manualSources });
          closeModal();
          page = 'tasks';
          tab = '待处理';
          quickView = '';
          selected = draft.id;
          await refreshState(true);
          toast('已添加，正在后台整理');
        } catch (err) {
          $('#modal-error').textContent = err.message;
          b.disabled = false;
        }
        break;
      }
      case 'retry-generation':
        await perform('generate', { id: t.id }, '正在重新生成');
        break;
      case 'save-task': {
        const edit = readTaskForm();
        b.disabled = true;
        try {
          const saved = await rpc('edit', { task: edit });
          closeModal();
          page = 'tasks';
          selected = saved.id;
          await refreshState(true);
          toast('事项已保存');
        } catch (err) {
          $('#modal-error').textContent = err.message;
          b.disabled = false;
        }
        break;
      }
      case 'done':
      case 'reopen':
      case 'dismiss':
      case 'unlock':
        await perform('action', { id: t.id, action }, '已更新事项');
        break;
      case 'merge':
        mergeForm(t);
        break;
      case 'apply-merge':
        await applyMerge();
        break;
      case 'prompt': {
        const sources = await rpc('evidence', { id: t.id });
        const prompt = `请协助排查：${t.title}\n问题描述：${t.description}\n当前阻塞：${t.blocker || '无'}\n\n相关对话：\n${sources.map((m) => `${m.group} / ${m.sender} / ${date(m.time)}\n${readableMessage(m.text).text}`).join('\n\n')}\n\n建议起点：${t.next_step || '先确认现象和复现步骤'}\n请先只读排查，区分事实与推测；缺少资料时明确列出。任何变更操作先说明具体内容。`;
        setModal(
          '<h2>排查提示词</h2><div class="subtitle">复制到所选 AI 工具使用，事项状态保持不变。</div><div class="form-group"><textarea id="prompt-text" class="prompt"></textarea></div><div class="dialog-foot"><button data-action="close">关闭</button><button class="primary" data-action="copy">复制</button></div>',
        );
        $('#prompt-text').value = prompt;
        break;
      }
      case 'copy':
        $('#prompt-text').select();
        try {
          await navigator.clipboard.writeText($('#prompt-text').value);
          toast('已复制');
        } catch {
          document.execCommand('copy');
          toast('已选中提示词，可按 Ctrl+C 复制');
        }
        break;
      case 'open-guide': {
        if (!(await leaveSettings())) break;
        page = 'guide';
        render();
        break;
      }

      case 'backlog':
        await openBacklog();
        break;
      case 'backlog-preview':
        await previewBacklog();
        break;
      case 'backlog-analyze': {
        if (state.ai_busy || state.analyzing) break;
        b.disabled = true;
        closeBacklog();
        const result = await perform('analyze', {}, '');
        toast(result?.skipped || '分析已完成');
        break;
      }
      case 'backlog-apply': {
        const preview = backlogPreview;
        if (!preview || !preview.count) break;
        b.disabled = true;
        const result = await rpc('analysis_backlog', { ...preview, apply: true });
        closeBacklog();
        await refreshState(true);
        toast(`已跳过 ${result.skipped_count} 条，原始消息仍保留`);
        break;
      }
      case 'save-settings': {
        await flushSettings();
        break;
      }
      case 'codex-status': {
        b.disabled = true;
        try {
          const r = await rpc('codex_status');
          setModal(
            `<h2>Codex 登录状态</h2><pre style="white-space:pre-wrap">${esc(r.status)}</pre><div class="dialog-foot"><button data-action="close">关闭</button></div>`,
          );
        } finally {
          b.disabled = false;
        }
        break;
      }
      case 'refresh-messages':
      case 'scan':
      case 'analyze':
      case 'report':
      case 'archive':
      case 'learn': {
        b.disabled = true;
        const original = b.innerHTML;
        b.setAttribute('aria-busy', 'true');
        b.innerHTML = b.classList.contains('icon-button')
          ? '<span class="refresh-glyph rotating" aria-hidden="true">↻</span>'
          : '<span class="spin"></span>' + b.textContent;
        try {
          const r = await perform(action === 'refresh-messages' ? 'scan_only' : action, {}, '');
          toast(
            r?.skipped ||
              r?.analysis?.skipped ||
              (action === 'scan'
                ? '刷新与 AI 分析已完成'
                : action === 'refresh-messages'
                  ? '消息已刷新'
                  : '处理完成'),
          );
        } finally {
          b.disabled = false;
          b.innerHTML = original;
          b.removeAttribute('aria-busy');
        }
        break;
      }
      case 'ai-interrupt': {
        b.disabled = true;
        try {
          const r = await rpc('ai_interrupt', { request_id: b.dataset.requestId });
          await refreshState(false);
          toast(r.skipped || '正在取消本次任务');
        } finally {
          b.disabled = !!state?.ai_cancelling;
        }
        break;
      }

      case 'open-data':
        await rpc('open_data');
        break;
    }
  } catch (err) {
    toast(err.message);
  }
});
document.addEventListener('compositionend', (e) => {
  if (e.target.id === 'inbox-search') {
    inboxQuery = e.target.value;
    renderInbox();
  }
});
document.addEventListener('pointerdown', (e) => {
  if (
    $('#backlog-panel') &&
    !e.target.closest('#backlog-panel') &&
    !e.target.closest('[data-action="backlog"]')
  )
    closeBacklog();
});
document.addEventListener('focusout', (e) => {
  const panel = $('#backlog-panel');
  if (
    panel?.contains(e.target) &&
    !panel.contains(e.relatedTarget) &&
    !e.relatedTarget?.closest('[data-action="backlog"]')
  )
    closeBacklog();
});
window.addEventListener('blur', closeBacklog);
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') {
    closeBacklog();
    closeModal();
  }
});
$('#overlay').addEventListener('click', (e) => {
  if (e.target === $('#overlay') && modalFromTrace) closeModal();
});
