document.addEventListener('click', async (e) => {
  const b = e.target.closest('button');
  if (!b) return;
  try {
    if (b.dataset.taskStatusValue) {
      const group = b.closest('[role="radiogroup"]');
      $('#f-status').value = b.dataset.taskStatusValue;
      group.querySelectorAll('[role="radio"]').forEach(button => {
        const selected = button === b;
        button.setAttribute('aria-checked', String(selected));
        button.tabIndex = selected ? 0 : -1;
      });
      return;
    }
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
      await toggleSchedule(b.dataset.scheduleToggle);
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
    if (b.dataset.view && !(await leaveSettings())) return;
    if (b.dataset.view) {
      page = 'tasks';
      tab = taskFocusViews[b.dataset.view] || '全部';
      quickView = taskFocusViews[b.dataset.view] ? '' : b.dataset.view;
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
      newModal(String(m?.text || ''), [b.dataset.build]);
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
        const task = readTaskForm();
        await modalRequest(b, 'edit_ai', { task, text }, async saved => {
          closeModal();
          page = 'tasks';
          selected = saved.id;
          await refreshState(true);
          toast('已根据你的说明更新当前事项');
        }, { label: '正在分析并更新…', lockForm: true });
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
        await modalRequest(b, 'schedule_format', { text }, parsed => {
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
        }, { label: '正在填写…', lockForm: true });
        break;
      }
      case 'save-schedule':
      case 'delete-schedule': {
        const saving = action === 'save-schedule';
        const params = saving ? { schedule: readScheduleForm() } : { id: modalSchedule.id, updated: modalSchedule.updated };
        await modalRequest(b, saving ? 'schedule_edit' : 'schedule_delete', params, async () => {
          closeModal();
          if (saving) page = 'schedules';
          await refreshState(true);
          toast(saving ? '定时事项已保存' : '已删除定时规则，已生成事项保留');
        });
        break;
      }
      case 'defer': {
        b.disabled = true;
        try { await perform('action', {id:t.id, updated:t.updated, action:'defer', choice:b.dataset.deferChoice || 'tomorrow'}, '已延后处理'); }
        finally { b.disabled = false; }
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
        await modalRequest(b, 'archive_task', {
          id: modalTask.id,
          updated: modalTask.updated,
          status: $('#archive-status').value,
          note: $('#archive-note').value,
        }, async () => {
          closeModal();
          await refreshState(true);
          toast('归档已保存，等待晚间或手动归档分析');
        });
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
        await modalRequest(b, 'generate', { text, sources: manualSources }, async draft => {
          closeModal();
          page = 'tasks';
          tab = '待处理';
          quickView = '';
          selected = draft.id;
          await refreshState(true);
          toast('已添加，正在后台整理');
        });
        break;
      }
      case 'retry-generation':
        await perform('generate', { id: t.id }, '正在重新生成');
        break;
      case 'save-task': {
        const edit = readTaskForm();
        await modalRequest(b, 'edit', { task: edit }, async saved => {
          closeModal();
          page = 'tasks';
          selected = saved.id;
          await refreshState(true);
          toast('事项已保存');
        });
        break;
      }
      case 'start':
      case 'done':
      case 'reopen':
      case 'dismiss':
      case 'unlock':
        await perform('action', { id: t.id, updated:t.updated, action }, '已更新事项');
        break;
      case 'merge':
        mergeForm(t);
        break;
      case 'apply-merge':
        await applyMerge();
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
        const scanning = action === 'scan' || action === 'refresh-messages';
        if (scanning && (scanPending || state.busy || (action === 'scan' && state.ai_busy))) break;
        if (scanning) scanPending = true;
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
          b.innerHTML = original;
          b.removeAttribute('aria-busy');
          if (scanning) {
            scanPending = false;
            status();
          } else {
            b.disabled = false;
          }
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
let modalPointerStartedOutside = false;
$('#overlay').addEventListener('pointerdown', (e) => {
  modalPointerStartedOutside = e.button === 0 && e.target === $('#overlay');
});
$('#overlay').addEventListener('pointercancel', () => { modalPointerStartedOutside = false; });
$('#overlay').addEventListener('click', (e) => {
  // Selecting text may end outside the dialog; that gesture is not an outside click.
  const outsideClick = e.detail === 0 || modalPointerStartedOutside;
  modalPointerStartedOutside = false;
  if (e.target === $('#overlay') && outsideClick && (modalFromTrace || modalOutsideDismiss?.())) closeModal();
});
