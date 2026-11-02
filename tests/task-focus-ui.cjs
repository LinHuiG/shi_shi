// Synthetic IPC and a Shanghai clock; no provider or installed data access.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const { withBrowser } = require('./ui-driver.cjs');
const at = Date.parse('2026-12-31T23:59:00+08:00');
const until = Date.parse('2027-01-01T08:00:00+08:00');
const task = (id, extra = {}) => ({
  id, title: 'Synthetic ' + id, description: 'Synthetic context', status: '待处理',
  created: at, updated: at, tags: [], ...extra,
});
const fixture = {
  tasks: [task('pending'), task('blank', { blocker: ' \t', waiting_for: ' \n' }),
    task('processing', { status: '处理中' }), task('waiting', { waiting_for: 'Reply' }),
    task('blocked', { status: '处理中', blocker: 'Blocked' }),
    task('both', { blocker: 'Blocked', waiting_for: 'Reply' }),
    task('deferred', { deferred_until: until }),
    task('deferred-blocked', { blocker: 'Blocked', deferred_until: until, resume_processing: true }),
    task('expired', { deferred: true, deferred_until: at - 1 }),
    task('expired-blocked', { status: '处理中', blocker: 'Blocked', deferred_until: at }),
    task('done', { status: '已完成', deferred_until: until }),
    task('cancelled', { status: '已取消' }), task('dismissed', { status: '无需处理' }),
    task('archived', { archived: true }), task('merged', { merged_into: 'pending' })],
  config: { enabled: false, auto_ai: false, leaders: [], interval_minutes: 5,
    daily_calls: 40, work_start: '08:00', work_end: '18:00' },
  messages: [], schedules: [], handling: [], activity: [], audit: [], reports: [],
  suggestions: [], chat_sources: [], usage: {},
};
const mock = `window.fixture=${JSON.stringify(fixture)};window.focusNow=${at};Date.now=()=>focusNow;
window.requests=[];window.ipc={postMessage(raw){const r=JSON.parse(raw);requests.push(r);setTimeout(()=>{
let data=true;if(r.method==='state')data=structuredClone(fixture);if(r.method==='evidence')data=[];
if(r.method==='action'){const t=fixture.tasks.find(t=>t.id===r.params.id);if(r.params.action==='start'){t.status='处理中';t.deferred_until=0;t.resume_processing=false;}if(r.params.action==='done')t.status='已完成';t.updated++;}
receive({id:r.id,ok:true,data});},5)}};`;

withBrowser(async(browser)=>{
    const page = await browser.newPage({ viewport: { width: 1280, height: 900 },
      timezoneId: 'America/Los_Angeles', reducedMotion: 'reduce' });
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.setContent(require('./ui-bundle.cjs')().replace('<script>', '<script>' + mock));
    await page.locator('.task-row').first().waitFor();
    const ids = () => page.locator('.task-row').evaluateAll(rows => rows.map(row => row.dataset.task).sort());
    const expected = {
      pending: ['blank', 'expired', 'pending'], processing: ['processing'],
      blocked: ['blocked', 'both', 'expired-blocked', 'waiting'],
      deferred: ['deferred', 'deferred-blocked'],
    };
    assert.equal(await page.locator('#sidebar-context .side-view').count(), 4);
    for (const [view, members] of Object.entries(expected)) {
      await page.locator(`[data-view="${view}"]`).click();
      assert.deepEqual(await ids(), members);
      assert.equal(await page.locator(`[data-view="${view}"] > span`).innerText(), String(members.length));
      assert.equal(await page.locator('[data-filter-group="view"] [aria-pressed="true"]').count(), 1);
    }
    assert.equal(await page.locator('#nav-count').textContent(), '3');
    assert.equal(await page.locator('#page-subtitle').textContent(), '3 项待处理');
    assert.deepEqual(await page.evaluate(() => {
      const groups = taskFocusLabels.map(scope => state.tasks.filter(t => taskScopeMatches(t, scope)).map(t => t.id));
      return [groups.flat().length, new Set(groups.flat()).size, state.tasks.filter(deadlineActive).length];
    }), [10, 10, 10]);

    await page.locator('[data-task="deferred"]').click();
    assert((await page.locator('.deferred-note').innerText()).startsWith('明天 08:00 恢复'));
    assert((await page.locator('[data-task="deferred"]').innerText()).includes('明天 08:00 恢复'));
    await page.locator('[data-task-status-trigger]').hover();
    await page.locator('[data-defer-trigger]').hover();
    await page.locator('[data-defer-choice="tomorrow"]').focus();
    const requestsBefore = await page.evaluate(() => requests.length);
    await page.evaluate(() => { focusNow = Date.parse('2027-01-01T00:00:00+08:00'); });
    await page.waitForFunction(() => document.querySelector('.deferred-note')?.textContent.startsWith('今天 08:00 恢复'));
    assert((await page.locator('[data-task="deferred"]').innerText()).includes('今天 08:00 恢复'));
    assert.equal(await page.locator('[data-defer-trigger]').getAttribute('aria-expanded'), 'true');
    assert.equal(await page.evaluate(() => document.activeElement.dataset.deferChoice), 'tomorrow');
    assert.equal(await page.locator('[data-defer-choice="tomorrow"] .tiny').textContent(), '明天 08:00');
    assert(await page.locator('[data-defer-choice="afternoon"]').isVisible(), 'midnight adds the new day fixed-time choices');
    assert.equal(await page.evaluate(() => requests.length), requestsBefore, 'clock refresh must not issue IPC');
    assert.deepEqual(await page.evaluate(() => {
      const oldStatus = status, oldRender = renderTasks; let statuses = 0, renders = 0;
      status = () => { statuses++; }; renderTasks = () => { renders++; };
      try { for (let i = 0; i < 100; i++) refreshTaskClock(); return [statuses, renders]; }
      finally { status = oldStatus; renderTasks = oldRender; }
    }), [0, 0], 'ordinary ticks do not scan tasks or redraw');

    const labels = await page.evaluate(() => {
      const at = Date.parse('2026-10-28T12:00:00+08:00');
      return ['2026-10-28', '2026-10-29', '2026-10-30', '2026-10-31', '2026-11-02', '2026-11-16']
        .map(day => taskDeferralTime(Date.parse(day + 'T08:05:00+08:00'), at));
    });
    assert.deepEqual(labels, ['今天 08:05', '明天 08:05', '后天 08:05', '本周六 08:05', '下周一 08:05', '3周后周一 08:05']);
    assert.deepEqual(await page.evaluate(() => [
      taskDeferralTime(Date.parse('2027-01-01T08:00:00+08:00'), Date.parse('2026-12-31T23:59:00+08:00')),
      taskDeferralTime(Date.parse('2028-03-01T08:00:00+08:00'), Date.parse('2028-02-28T23:59:00+08:00')),
    ]), ['明天 08:00', '后天 08:00']);

    // Hidden windows skip work; the next visible tick catches up using the current date.
    assert.deepEqual(await page.evaluate(() => {
      Object.defineProperty(document, 'hidden', { configurable: true, value: true });
      focusNow = Date.parse('2027-01-01T08:00:00+08:00'); refreshTaskClock();
      const hiddenCount = document.querySelectorAll('.task-row').length;
      delete document.hidden; refreshTaskClock();
      return [hiddenCount, visibleTasks().length];
    }), [2, 0]);
    assert.equal(await page.locator('[data-view="deferred"] > span').innerText(), '0');
    assert.equal(await page.locator('[data-view="blocked"] > span').innerText(), '5');

    // Manual start clears deferral without changing the blocker, then a reply clears the blocker.
    await page.evaluate(async () => { const t = fixture.tasks.find(t => t.id === 'deferred-blocked');
      t.deferred_until = focusNow + 1000; await refreshState(true); });
    await page.locator('[data-view="deferred"]').click();
    await page.locator('[data-task-status-trigger]').click();
    await page.waitForFunction(() => state.tasks.find(t => t.id === 'deferred-blocked').status === '处理中' && !state.tasks.find(t => t.id === 'deferred-blocked').deferred_until);
    await page.locator('[data-view="blocked"]').click();
    assert((await ids()).includes('deferred-blocked'));
    await page.evaluate(async () => { fixture.tasks.find(t => t.id === 'deferred-blocked').blocker = ''; await refreshState(true); });
    await page.locator('[data-view="processing"]').click();
    assert.deepEqual(await ids(), ['deferred-blocked', 'processing']);
    const unchanged = await page.evaluate(() => JSON.stringify(fixture.tasks));
    await page.evaluate(() => { focusNow += 1000; refreshTaskClock(); refreshTaskClock(); });
    assert.equal(await page.evaluate(() => JSON.stringify(fixture.tasks)), unchanged, 'stale wakeup cannot restore a cancelled deferral');

    // An open submenu updates in place at each minute and drops expired fixed-time choices.
    for (const [before, after, choice] of [
      ['12:59', '13:00', 'afternoon'], ['16:59', '17:00', 'evening'],
    ]) {
      await page.evaluate(time => { focusNow = Date.parse(`2027-01-01T${time}:00+08:00`); renderTasks(); }, before);
      await page.locator('[data-task-status-trigger]').hover();
      await page.locator('[data-defer-trigger]').hover();
      const option = page.locator(`[data-defer-choice="${choice}"]`);
      assert(await option.isVisible()); await option.focus();
      assert.equal(await page.evaluate(() => document.activeElement.dataset.deferChoice), choice);
      await page.evaluate(time => { focusNow = Date.parse(`2027-01-01T${time}:00+08:00`); }, after);
      await option.waitFor({ state: 'hidden' });
      assert.equal(await page.evaluate(() => document.activeElement.hasAttribute('data-defer-trigger')), true,
        JSON.stringify(await page.evaluate(() => ({ active: document.activeElement.tagName,
          choice: document.activeElement.dataset.deferChoice,
          expanded: document.querySelector('[data-defer-trigger]').getAttribute('aria-expanded'),
          inert: document.querySelector('.task-defer-popover').inert }))));
      const times = await page.locator('[data-defer-choice]:visible').evaluateAll(buttons => buttons.map(b => ({ title: b.title, text: b.querySelector('.tiny').textContent })));
      assert(times.every(value => value.title === value.text + ' 恢复' && !/\d{4}[/-]/.test(value.text)));
    }
    // Sorting happens before display: defer time wins within deferred rows,
    // while regular rows keep their original priority/update order and slots.
    await page.evaluate(() => {
      const at = Date.now();
      state.tasks = [
        ['low', 1, 30, 0], ['older', 0, 10, 0], ['newer', 0, 20, 0], ['none', 9, 99, 0],
        ['far', 0, 40, at + 2 * 86400000], ['early', 1, 25, at + 3600000],
        ['tie-old', 0, 15, at + 3600000], ['tie-new', 0, 19, at + 3600000],
      ].map(([id, priority_rank, updated, deferred_until]) => ({
        id, title: 'Synthetic ' + id, status: '待处理', tags: [], priority_rank, updated, deferred_until,
      }));
      tab = '全部'; quickView = ''; query = ''; selectedTags = {}; selectedPriorities = [];
      status(); renderTasks();
    });
    const orderedIds = () => page.locator('.task-row').evaluateAll(rows => rows.map(row => row.dataset.task));
    const mixedOrder = ['tie-new', 'newer', 'tie-old', 'early', 'older', 'low', 'far', 'none'];
    assert.deepEqual(await orderedIds(), mixedOrder);
    assert.deepEqual(await page.evaluate(() => {
      const before = JSON.stringify(state.tasks);
      for (let i = 0; i < 20; i++) visibleTasks();
      return [before === JSON.stringify(state.tasks), visibleTasks().map(t => t.id)];
    }), [true, mixedOrder], 'sorting does not mutate shared state and repeated results stay stable');
    await page.evaluate(() => { tab = '延后处理'; renderTasks(); });
    assert.deepEqual(await orderedIds(), ['tie-new', 'tie-old', 'early', 'far']);
    await page.evaluate(() => {
      state.tasks.find(t => t.id === 'far').deferred_until = Date.now() + 1800000;
      status(); renderTasks();
    });
    assert.deepEqual(await orderedIds(), ['far', 'tie-new', 'tie-old', 'early']);
    await page.evaluate(() => {
      state.tasks.find(t => t.id === 'early').deferred_until = 0;
      focusNow += 1800000;
      refreshTaskClock();
    });
    assert.deepEqual(await orderedIds(), ['tie-new', 'tie-old']);
    await page.evaluate(() => { tab = '待处理'; renderTasks(); });
    assert.deepEqual(await orderedIds(), ['far', 'newer', 'older', 'low', 'early', 'none']);

    // Display numbers join the ordinary text search; a hash is not a special operator.
    await page.evaluate(() => {
      fixture.tasks = [
        { id: 'visible-number', number: 141, updated: 10, deferred_until: Date.now() + 3600000 },
        { id: 'number-in-text', number: 179, updated: 30, description: 'Reference 141055' },
        { id: 'hash-in-text', number: 180, updated: 20, description: 'Related #141 entry' },
        { id: 'other', number: 200, updated: 0 },
        { id: 'archived-number', number: 1410, updated: 40, archived: true },
      ].map(t => ({ title: 'Synthetic task', description: 'Neutral context', blocker: '',
        status: '待处理', tags: [], ...t }));
      state.tasks = structuredClone(fixture.tasks);
      tab = '全部'; quickView = ''; query = ''; selectedTags = {}; selectedPriorities = [];
      status(); renderTasks();
    });
    const search = page.locator('#task-search');
    await search.fill('141');
    assert.deepEqual(await orderedIds(), ['number-in-text', 'hash-in-text', 'visible-number']);
    assert((await page.locator('[data-task="visible-number"]').innerText()).includes('#141'));
    for (const text of ['#141', '#14']) {
      await search.fill(text);
      assert.deepEqual(await orderedIds(), ['hash-in-text', 'visible-number']);
    }
    await search.fill('141055');
    assert.deepEqual(await orderedIds(), ['number-in-text']);
    await search.fill('141');
    await page.locator('[data-view="deferred"]').click();
    assert.deepEqual(await orderedIds(), ['visible-number']);
    assert.equal(await search.inputValue(), '141');
    await page.locator('[data-view="pending"]').click();
    assert.deepEqual(await orderedIds(), ['number-in-text', 'hash-in-text']);
    await search.fill('no-match');
    assert.deepEqual(await orderedIds(), []);
    assert.equal(await page.evaluate(() => tab), '待处理', 'search does not clear the selected scope');
    await page.evaluate(() => { tab = '归档'; quickView = ''; query = '#14'; renderTasks(); });
    assert.deepEqual(await orderedIds(), ['archived-number']);
    assert.deepEqual(await page.evaluate(() => state.tasks.map(t => t.number)), [141, 179, 180, 200, 1410]);
    assert.deepEqual(errors, []);
    console.log('PASS four exclusive focus views, count/query consistency, action transitions, Shanghai relative dates, midnight, hidden/resumed and bounded clock refresh, live submenu expiry, deferred wakeup sorting, stable mixed rows and expired/cancelled deferrals, display-number and ordinary-text search within the selected scope');
  }).catch(error => { console.error(error); process.exitCode = 1; });
