let selectedTags = null,
  selectedPriorities = null,
  tagExpanded = new Set(),
  tagSave = Promise.resolve(),
  tagSavePending = 0;
const tagOrder = new Map();
function rememberTagOrder(group) {
  const key = group?.dataset.filterGroup;
  if (key && !tagOrder.has(key))
    tagOrder.set(
      key,
      [...group.querySelectorAll('.tag-option')].map(
        (b) => b.dataset.tagValue || b.dataset.tagPriority || b.dataset.tab,
      ),
    );
}
function orderedTagValues(group) {
  const order = tagOrder.get(group.key);
  return [...group.values].sort(
    order
      ? (a, b) =>
          (order.includes(a) ? order.indexOf(a) : order.length) -
          (order.includes(b) ? order.indexOf(b) : order.length)
      : (a, b) => Number(group.selected.includes(b)) - Number(group.selected.includes(a)),
  );
}
function tagSelection() {
  if (selectedTags === null) selectedTags = structuredClone(state?.tag_state?.filters || {});
  return selectedTags;
}
function prioritySelection() {
  if (selectedPriorities === null) selectedPriorities = [...(state?.tag_state?.priorities || [])];
  return selectedPriorities;
}
function legacyTaskTags(t) {
  return [
    ...(t.tags || []),
    ...(t.category ? [{ kind: '事项类型', value: t.category }] : []),
    ...(t.environment && !['未指定', '不适用'].includes(t.environment)
      ? [{ kind: '适用环境', value: t.environment }]
      : []),
    ...(t.priority ? [{ kind: '事项等级', value: t.priority }] : []),
  ];
}
function taskTags(t) {
  return [
    ...new Map(
      (state.tag_state?.unified ? t.tags || [] : legacyTaskTags(t)).map((tag) => [
        JSON.stringify([tag.kind, tag.value]),
        tag,
      ]),
    ).values(),
  ];
}
function displayTagPriority(g) {
  return Number.isInteger(g.display_priority)
    ? g.display_priority
    : g.name === priorityKind()
      ? 10
      : g.name === '事项类型'
        ? 20
        : 100;
}
function orderedGroups(groups) {
  return [...groups].sort((a, b) => displayTagPriority(a) - displayTagPriority(b));
}
function shownTags(t) {
  const tags = t.effective_tags || taskTags(t),
    groups = orderedGroups(state.tag_state?.groups || []),
    visible = state.tag_state?.unified
      ? tags.filter((t) => groups.some((g) => g.name === t.kind && g.values.includes(t.value)))
      : tags;
  return [...visible].sort((a, b) => {
    const rank = (k) => {
      const g = groups.find((g) => g.name === k);
      return g ? displayTagPriority(g) : 100;
    };
    return rank(a.kind) - rank(b.kind);
  });
}
function priorityKind() {
  return state.tag_state?.unified ? state.tag_state.priority_kind || '' : '事项等级';
}
function taskRank(t) {
  if (Number.isFinite(t.priority_rank)) return t.priority_rank;
  const values = filterGroups().find((g) => g.name === priorityKind())?.values || [],
    ranks = shownTags(t)
      .filter((tag) => tag.kind === priorityKind())
      .map((tag) => values.indexOf(tag.value))
      .filter((v) => v >= 0);
  return ranks.length ? Math.min(...ranks) : Number.MAX_SAFE_INTEGER;
}
function tagMatches(t) {
  const tags = shownTags(t),
    priorities = prioritySelection();
  return (
    (!priorities.length ||
      tags.some((t) => t.kind === priorityKind() && priorities.includes(t.value))) &&
    Object.entries(tagSelection()).every(
      ([kind, values]) =>
        !values.length ||
        values.some((value) => tags.some((t) => t.kind === kind && t.value === value)),
    )
  );
}
function revealTaskTags(task) {
  const tags = shownTags(task), filters = tagSelection();
  let changed = false;
  for (const [kind, values] of Object.entries(filters)) {
    if (values.length && !tags.some(tag => tag.kind === kind && values.includes(tag.value))) {
      delete filters[kind];
      changed = true;
    }
  }
  const priorities = prioritySelection();
  if (priorities.length && !tags.some(tag => tag.kind === priorityKind() && priorities.includes(tag.value))) {
    selectedPriorities = [];
    changed = true;
  }
  return changed;
}
function filterGroups() {
  const groups = structuredClone(state.tag_state?.groups || []);
  if (!state.tag_state?.unified) {
    if (!groups.some((g) => g.name === '事项等级'))
      groups.unshift({ name: '事项等级', values: ['紧急', '高', '普通', '低'] });
    for (const t of state.tasks) for (const tag of taskTags(t)) add(tag);
    for (const [kind, values] of Object.entries(tagSelection()))
      for (const value of values) add({ kind, value });
  }
  function add(t) {
    let g = groups.find((g) => g.name === t.kind);
    if (!g) {
      g = { name: t.kind, values: [] };
      groups.push(g);
    }
    if (!g.values.includes(t.value)) g.values.push(t.value);
  }
  return orderedGroups(groups);
}
function tagFilterBar() {
  const selection = tagSelection(),
    priorities = prioritySelection(),
    active =
      Object.values(selection).some((a) => a.length) ||
      priorities.length ||
      tab !== '全部' ||
      !!quickView;
  const groups = [
    {
      name: '事项范围',
      key: 'view',
      values: ['待处理', '已完成', '归档'],
      selected: tab === '全部' ? [] : [tab],
    },
    ...filterGroups().map((g) => ({
      ...g,
      key: 'tag:' + g.name,
      selected: selection[g.name] || [],
    })),
  ];
  return `<div class="tag-filters" aria-label="事项筛选" title="同类型任选其一，不同类型同时满足"><button class="tag-all ${active ? '' : 'selected'}" data-tag-clear aria-pressed="${!active}" title="清空范围、标签和等级筛选">全部</button><div class="tag-groups">${groups
    .map(
      (g) =>
        `<div class="tag-slot" data-filter-slot="${esc(g.key)}"><section class="tag-group" data-filter-group="${esc(g.key)}"><button class="tag-group-title" data-tag-expand="${esc(g.name)}" data-filter-key="${esc(g.key)}" aria-expanded="false"><span>${esc(g.name)}</span><small>${g.key === 'view' ? '' : g.selected.length || ''}</small><span class="tag-chevron">＋</span></button><div class="tag-options">${orderedTagValues(
          g,
        )
          .map(
            (value) =>
              `<button class="tag-option ${g.selected.includes(value) ? 'selected' : ''} ${g.key === 'view' && tab === value ? 'active' : ''}" ${g.key === 'view' ? `data-tab="${esc(value)}"` : g.key === 'priority' ? `data-tag-priority="${esc(value)}"` : `data-tag-kind="${esc(g.name)}" data-tag-value="${esc(value)}"`} aria-pressed="${g.selected.includes(value)}">${esc(value)}</button>`,
          )
          .join('')}</div></section></div>`,
    )
    .join(
      '',
    )}</div><input id="task-search" aria-label="搜索事项" placeholder="搜索事项" value="${esc(query)}"></div>`;
}
function rememberTagBar() {
  const el = document.activeElement;
  return {
    focus: el?.closest('.tag-group') ? { ...el.dataset } : null,
    options: $('.tag-group.expanded .tag-options')?.scrollTop || 0,
  };
}
function restoreTagBar(saved) {
  const bar = $('.tag-groups');
  if (!bar) return;
  for (const group of bar.querySelectorAll('.tag-group')) {
    const expanded = tagExpanded.has(group.dataset.filterGroup),
      title = group.querySelector('.tag-group-title'),
      wasExpanded = group.classList.contains('expanded');
    const slot = group.parentElement;
    if (expanded && !wasExpanded) slot.style.width = slot.getBoundingClientRect().width + 'px';
    group.classList.toggle('expanded', expanded);
    title.setAttribute('aria-expanded', String(expanded));
    group.querySelector('.tag-chevron').textContent = expanded ? '−' : '＋';
    if (expanded) {
      const rect = group.parentElement.getBoundingClientRect(),
        width = Math.min(360, window.innerWidth - 24);
      Object.assign(group.style, {
        left: Math.max(12, Math.min(rect.left, window.innerWidth - width - 12)) + 'px',
        top: rect.top + 'px',
        width: width + 'px',
      });
      group.querySelector('.tag-options').style.maxHeight =
        Math.max(70, Math.min(240, window.innerHeight - rect.top - 65)) + 'px';
      group.querySelector('.tag-options').scrollTop = saved.options;
      if (!wasExpanded) motionAnimate(group, [{ opacity: 0.4 }, { opacity: 1 }]);
    } else {
      group.removeAttribute('style');
      slot.style.removeProperty('width');
    }
  }
  if (saved.focus) {
    const el = [...bar.querySelectorAll('button')].find((el) =>
      Object.entries(saved.focus).every(([k, v]) => el.dataset[k] === v),
    );
    el?.focus({ preventScroll: true });
  }
  observeTagOverflow();
}
function updateTagOverflow(options) {
  options.classList.toggle('has-more-options', options.scrollWidth - options.clientWidth - options.scrollLeft > 1);
}
const tagOverflowObserver = typeof ResizeObserver === 'function'
  ? new ResizeObserver(entries => entries.forEach(entry => updateTagOverflow(entry.target))) : null;
function tagOptionsScrolled(event) { updateTagOverflow(event.currentTarget); }
function observeTagOverflow() {
  tagOverflowObserver?.disconnect();
  for (const options of document.querySelectorAll('.tag-options')) {
    updateTagOverflow(options);
    tagOverflowObserver?.observe(options);
    options.addEventListener('scroll', tagOptionsScrolled, { passive: true });
  }
}
function collapseTagGroups() {
  if (!tagExpanded.size && !tagOrder.size) return;
  tagExpanded.clear();
  tagOrder.clear();
  restoreTagBar({ options: 0 });
  for (const options of document.querySelectorAll('.tag-options'))
    for (const b of [...options.children].sort(
      (a, b) => Number(b.classList.contains('selected')) - Number(a.classList.contains('selected')),
    ))
      options.append(b);
  observeTagOverflow();
}
function saveTagFilters() {
  const filters = structuredClone(tagSelection()),
    priorities = [...prioritySelection()];
  tagSavePending++;
  tagSave = tagSave
    .catch(() => {})
    .then(() => rpc('tag_filters', { filters, priorities }))
    .then((saved) => {
      if (state.tag_state) {
        state.tag_state.filters = saved;
        state.tag_state.priorities = priorities;
      }
    })
    .catch((e) => toast('筛选保存失败：' + e.message))
    .finally(() => {
      tagSavePending--;
    });
}
function focusTagGroup(group) {
  if (!group) {
    collapseTagGroups();
    return;
  }
  if ([...tagOrder.keys()].some((key) => key !== group.dataset.filterGroup)) collapseTagGroups();
  rememberTagOrder(group);
}
document.addEventListener('pointerdown', (e) => focusTagGroup(e.target.closest('.tag-group')));
document.addEventListener('focusin', (e) => focusTagGroup(e.target.closest('.tag-group')));
document.addEventListener('focusout', (e) => {
  if (e.target.closest('.tag-group'))
    setTimeout(() => {
      if (!document.activeElement?.closest('.tag-group')) collapseTagGroups();
    }, 0);
});
document.addEventListener('pointerout', (e) => {
  const group = e.target.closest('.tag-group');
  if (group?.isConnected && !group.contains(e.relatedTarget)) collapseTagGroups();
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') collapseTagGroups();
});
window.addEventListener('blur', collapseTagGroups);
window.addEventListener('resize', collapseTagGroups);
function extraTagBadges(t) {
  return shownTags(t)
    .map(
      (tag) =>
        `<span class="badge tag-extra ${tag.kind === priorityKind() ? ['urgent', 'high', 'normal', 'low'][Math.min(3, taskRank(t))] || '' : ''}" title="${esc(tag.kind)}">${esc(tag.value)}</span>`,
    )
    .join('');
}
function taskTagEditor(t, collapsible = false) {
  const groups = filterGroups(), tags = taskTags(t),
    selected = groups.some(g => tags.some(tag => tag.kind === g.name && g.values.includes(tag.value))),
    fields = `${groups
    .map(
      (g) =>
        `<div class="task-tag-field"><span>${esc(g.name)}</span><div class="row">${g.values.map((v) => `<label class="check"><input type="checkbox" data-edit-tag-kind="${esc(g.name)}" ${g.name === priorityKind() ? 'data-edit-tag-single="1"' : ''} value="${esc(v)}" ${taskTags(t).some((x) => x.kind === g.name && x.value === v) ? 'checked' : ''}>${esc(v)}</label>`).join('')}</div></div>`,
    )
    .join('')}<div class="helper">新枚举由 AI 从消息学习，在迭代建议中预览并采用。</div>`;
  return collapsible
    ? `<details class="form-group task-tag-settings" ${selected ? 'open' : ''}><summary>标签（可留空）</summary>${fields}</details>`
    : `<div class="form-group"><label>标签（可留空）</label>${fields}</div>`;
}
function readTaskTags() {
  return [...document.querySelectorAll('[data-edit-tag-kind]:checked')].map((el) => ({
    kind: el.dataset.editTagKind,
    value: el.value,
  }));
}
function historicalTagBadges(t) {
  return [...new Map(legacyTaskTags(t).map((tag) => [JSON.stringify(tag), tag])).values()]
    .map((tag) => `<span class="badge tag-extra" title="${esc(tag.kind)}">${esc(tag.value)}</span>`)
    .join('');
}
function tagPriorityFields() {
  return `<div class="tag-display-settings"><p class="helper">数值越小越靠前，仅调整显示顺序。</p><div class="tag-priority-grid">${orderedGroups(
    state.tag_state?.groups || [],
  )
    .map(
      (g) =>
        `<label class="tag-priority-row"><span>${esc(g.name)}</span><input type="number" min="-10000" max="10000" step="1" aria-label="${esc(g.name)}显示优先级" data-tag-display-priority="${esc(g.name)}" value="${displayTagPriority(g)}"></label>`,
    )
    .join('')}</div></div>`;
}
document.addEventListener('change', async (e) => {
  const input = e.target;
  if (!input.hasAttribute('data-tag-display-priority')) return;
  const value = Number(input.value),
    group = state.tag_state.groups.find((g) => g.name === input.dataset.tagDisplayPriority);
  if (!group || value === displayTagPriority(group)) return;
  if (!input.value || !Number.isInteger(value) || Math.abs(value) > 10000) {
    toast('标签优先级需为 -10000 至 10000 的整数');
    return;
  }
  input.disabled = true;
  try {
    await rpc('tag_decide', {
      action: 'display_priority',
      name: group.name,
      display_priority: value,
      expected: state.tag_state.versions.at(-1).id,
    });
    await refreshState(true);
    toast('标签显示顺序已保存');
  } catch (error) {
    toast('保存失败：' + error.message);
  } finally {
    input.disabled = false;
  }
});
document.addEventListener('click', async (e) => {
  const b = e.target.closest('button');
  if (!b) return;
  if (b.hasAttribute('data-tag-clear') || b.dataset.tagKind || b.dataset.tagPriority) {
    const current = tagSelection();
    if (b.hasAttribute('data-tag-clear')) {
      selectedTags = {};
      selectedPriorities = [];
      tab = '全部';
      quickView = '';
    } else if (b.dataset.tagPriority) {
      const p = b.dataset.tagPriority,
        values = prioritySelection();
      selectedPriorities = values.includes(p) ? values.filter((v) => v !== p) : [...values, p];
    } else {
      const kind = b.dataset.tagKind,
        value = b.dataset.tagValue,
        values = current[kind] || [];
      current[kind] = values.includes(value)
        ? values.filter((v) => v !== value)
        : [...values, value];
      if (!current[kind].length) delete current[kind];
    }
    saveTagFilters();
    renderTasks();
    return;
  }
  if (b.dataset.tagExpand) {
    const kind = b.dataset.filterKey,
      expanded = tagExpanded.has(kind),
      saved = rememberTagBar();
    tagExpanded.clear();
    if (!expanded) tagExpanded.add(kind);
    restoreTagBar(saved);
    return;
  }
  if (!b.dataset.tagPreview && !b.dataset.tagDecision && !b.dataset.tagRollback) return;
  try {
    if (b.dataset.tagPreview) {
      const p = state.tag_state.proposals.find((p) => p.id === b.dataset.tagPreview);
      setModal(
        `<h2>标签变更建议</h2><p>${esc(p.reason)}</p><div class="cols"><div class="form-group"><label>类型</label><input id="tag-kind" maxlength="24" value="${esc(p.tag.kind)}"></div><div class="form-group"><label>枚举值</label><input id="tag-value" maxlength="40" value="${esc(p.tag.value)}"></div></div><h3 style="margin-top:20px">消息或修正依据</h3>${p.evidence.map((id, i) => `<div class="source"><div class="tiny">${esc(id)}</div><p>${esc(p.excerpts[i] || '')}</p></div>`).join('')}<p class="helper">可调整名称后采用。只扩展后续分析的枚举，不修改已有事项。每个采用版本均可回滚。</p><div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">稍后处理</button><button data-tag-decision="reject" data-id="${esc(p.id)}">忽略</button><button class="primary" data-tag-decision="adopt" data-id="${esc(p.id)}">采用标签</button></div>`,
      );
      return;
    }
    if (b.dataset.tagDecision || b.dataset.tagRollback) {
      b.disabled = true;
      await rpc('tag_decide', {
        action: b.dataset.tagDecision || 'rollback',
        id: b.dataset.id || b.dataset.tagRollback,
        expected: state.tag_state.versions.at(-1).id,
        ...(b.dataset.tagDecision === 'adopt'
          ? { tag: { kind: $('#tag-kind').value.trim(), value: $('#tag-value').value.trim() } }
          : {}),
      });
      closeModal();
      await refreshState(true);
      toast('标签建议已处理，历史版本已保留');
    }
  } catch (e) {
    if ($('#modal-error')) $('#modal-error').textContent = e.message;
    else toast(e.message);
  } finally {
    b.disabled = false;
  }
});

document.addEventListener('change', (e) => {
  if (e.target.dataset.editTagSingle && e.target.checked) {
    for (const input of document.querySelectorAll('[data-edit-tag-single]'))
      if (input !== e.target && input.dataset.editTagKind === e.target.dataset.editTagKind)
        input.checked = false;
  }
});
