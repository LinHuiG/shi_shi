// Synthetic IPC; inspect intermediate frames, not only the settled DOM.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const { withBrowser } = require('./ui-driver.cjs');

const now = Date.now();
const task = (id, number, status, extra = {}) => ({
  id, number, title: 'Synthetic ' + id, description: 'Neutral context', status,
  created: now, updated: now, tags: [], ...extra,
});
const fixture = {
  tasks: [task('pending-alpha', 101, '待处理'), task('processing-beta', 102, '处理中'),
    task('blocked-gamma', 103, '待处理', { blocker: 'Synthetic dependency' })],
  handling: [], schedules: [], messages: [], activity: [], audit: [], reports: [],
  suggestions: [], chat_sources: [], usage: {}, core_revision: '1',
  config: { enabled: false, auto_ai: false, leaders: [], ai_connections: [], interval_minutes: 3 },
  ai_busy: true, ai_request_id: 'synthetic-analysis',
  ai_live: { request_id: 'synthetic-analysis', phase: 'Synthetic analysis',
    entries: [{ id: 'progress', kind: 'content', text: 'Synthetic progress' }] },
};
const mock = `window.fixture=${JSON.stringify(fixture)};
window.requests=[];window.delayNextState=0;
window.ipc={postMessage(raw){
  const r=JSON.parse(raw);requests.push(r);let data=true,delay=0;
  if(r.method==='state'){
    delay=delayNextState;delayNextState=0;
    data=r.params.core_revision===fixture.core_revision
      ? {_unchanged:true,_page:r.params.page,core_revision:fixture.core_revision,
          ai_busy:fixture.ai_busy,ai_request_id:fixture.ai_request_id,ai_live:structuredClone(fixture.ai_live)}
      : {...structuredClone(fixture),_page:r.params.page};
  }
  if(r.method==='evidence')data=[];
  if(r.method==='messages_query')data={rows:[],page:1,pages:1,total:0};
  setTimeout(()=>receive({id:r.id,ok:true,data}),delay);
}};`;

withBrowser(async(browser)=>{
    const page = await browser.newPage({ viewport: { width: 1360, height: 820 } });
    const errors = [], failures = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.setContent(require('./ui-bundle.cjs')().replace('<script>', '<script>' + mock));
    await page.waitForFunction(() => requests.some(r => r.method === 'ui_ready'));
    const settle = () => page.evaluate(async () => {
      await Promise.all(document.getAnimations()
        .filter(animation => animation.effect.getTiming().iterations !== Infinity)
        .map(animation => animation.finished.catch(() => {})));
    });
    await settle();

    // Live output changes must not rewrite or fade an unchanged task count.
    const runtime = await page.evaluate(async () => {
      const subtitle = document.querySelector('#page-subtitle'), text = subtitle.firstChild,
        row = document.querySelector('.task-row'), pane = document.querySelector('.task-list');
      let mutations = 0;
      const observer = new MutationObserver(records => { mutations += records.length; });
      observer.observe(subtitle, { childList: true, characterData: true, subtree: true });
      const frames = [];
      for (let i = 0; i < 5; i++) {
        fixture.ai_live.entries[0].text = 'Synthetic progress ' + i;
        await refreshState(false);
        await new Promise(requestAnimationFrame);
        frames.push({ text: subtitle.textContent, opacity: Number(getComputedStyle(subtitle).opacity),
          textStable: text === subtitle.firstChild, rowStable: row === document.querySelector('.task-row'),
          paneStable: pane === document.querySelector('.task-list') });
      }
      observer.disconnect();
      return { mutations, frames, live: document.querySelector('[data-live-line]')?.textContent };
    });
    if (runtime.mutations || runtime.frames.some(frame => frame.text !== '1 项待处理' ||
      frame.opacity < 0.999 || !frame.textStable || !frame.rowStable || !frame.paneStable))
      failures.push({ case: 'unchanged count during live refresh', ...runtime });
    assert(runtime.live.includes('Synthetic progress 4'), 'live progress still advances');
    await settle();

    // Each action is observed immediately and on every following animation frame.
    // Ghosts deliberately have no task-row class, so inspect their visible text too.
    async function transition(label, action, expected, frames = 12) {
      const samples = await page.evaluate(async ({ action, expected, frames }) => {
        const toolbar = document.querySelector('.tag-filters'), split = document.querySelector('.split'),
          pane = document.querySelector('.task-list');
        if (action.search !== undefined) {
          const input = document.querySelector('#task-search');
          input.focus(); input.value = action.search;
          input.setSelectionRange(input.value.length, input.value.length);
          input.dispatchEvent(new Event('input', { bubbles: true }));
        } else document.querySelector(action.selector).click();
        await Promise.resolve();
        const samples = [];
        function opacityOf(node) {
          let opacity = 1;
          for (; node && node !== document.body; node = node.parentElement)
            opacity *= Number(getComputedStyle(node).opacity);
          return opacity;
        }
        function sample() {
          const list = document.querySelector('.task-list');
          const rows = [...document.querySelectorAll('.task-row')];
          const opacity = Math.min(opacityOf(list), opacityOf(document.querySelector('#task-detail h2')),
            ...rows.map(opacityOf));
          samples.push({ ids: [...document.querySelectorAll('.task-row')].map(row => row.dataset.task).sort(),
            ghosts: [...document.querySelectorAll('[data-motion-ghost]')]
              .filter(node => Number(getComputedStyle(node).opacity) > 0.001)
              .map(node => node.textContent).filter(text => text.includes('Synthetic ')),
            toolbarStable: toolbar === document.querySelector('.tag-filters'),
            shellStable: !pane || !expected.length ||
              (pane === list && split === document.querySelector('.split')), opacity,
            search: document.querySelector('#task-search').value });
        }
        sample();
        for (let i = 0; i < frames; i++) { await new Promise(requestAnimationFrame); sample(); }
        return samples;
      }, { action, expected, frames });
      const wanted = [...expected].sort();
      const bad = samples.find(sample => JSON.stringify(sample.ids) !== JSON.stringify(wanted) ||
        sample.ghosts.length || !sample.toolbarStable || !sample.shellStable || sample.opacity < 0.999 ||
        (action.search !== undefined && sample.search !== action.search));
      if (bad) failures.push({ case: label, expected: wanted, frame: bad });
    }

    await transition('pending to processing', { selector: '[data-tab="处理中"]' }, ['processing-beta']);
    await transition('processing to pending', { selector: '[data-tab="待处理"]' }, ['pending-alpha']);
    await transition('sidebar processing', { selector: '[data-view="processing"]' }, ['processing-beta']);
    await transition('processing to blocked', { selector: '[data-tab="被阻塞"]' }, ['blocked-gamma']);
    await transition('blocked to processing', { selector: '[data-tab="处理中"]' }, ['processing-beta']);
    for (let i = 0; i < 4; i++) {
      await transition('rapid pending ' + i, { selector: '[data-tab="待处理"]' }, ['pending-alpha'], 1);
      await transition('rapid processing ' + i, { selector: '[data-tab="处理中"]' }, ['processing-beta'], 1);
    }

    // A reply captured for the old selected task must not reset a newer filter.
    await page.evaluate(() => { delayNextState = 35; window.pendingSnapshot = refreshState(true); });
    await transition('filter while state response is in flight', { selector: '[data-tab="待处理"]' }, ['pending-alpha']);
    await page.evaluate(() => pendingSnapshot);
    await transition('all tasks', { selector: '[data-tag-clear]' }, ['pending-alpha', 'processing-beta', 'blocked-gamma']);
    await transition('search first result', { search: 'pending-alpha' }, ['pending-alpha']);
    await transition('search replaces result', { search: 'processing-beta' }, ['processing-beta']);
    await transition('search empty', { search: 'no synthetic match' }, []);
    await transition('search restores result', { search: 'blocked-gamma' }, ['blocked-gamma']);
    await transition('clear search', { search: '' }, ['pending-alpha', 'processing-beta', 'blocked-gamma']);
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await transition('reduced motion pending', { selector: '[data-tab="待处理"]' }, ['pending-alpha']);
    await transition('reduced motion processing', { selector: '[data-tab="处理中"]' }, ['processing-beta']);
    await page.emulateMedia({ reducedMotion: 'no-preference' });

    // Updates to this task preserve its reading position; another task starts at the top.
    await page.evaluate(async () => {
      for (const task of fixture.tasks) task.description = 'Synthetic long detail.\n'.repeat(160);
      fixture.core_revision = 'details'; await refreshState(true);
    });
    await settle();
    const detailScroll = await page.evaluate(async () => {
      const detail = document.querySelector('#task-detail'); detail.scrollTop = 350;
      const before = detail.scrollTop;
      fixture.tasks[1].description += 'Synthetic final line';
      fixture.core_revision = 'detail-update'; await refreshState(false);
      return { before, after: detail.scrollTop, retained: detail === document.querySelector('#task-detail') };
    });
    assert(detailScroll.before > 0 && detailScroll.retained && detailScroll.before === detailScroll.after,
      'same task retains its detail reading position: ' + JSON.stringify(detailScroll));
    await transition('another long detail', { selector: '[data-tab="待处理"]' }, ['pending-alpha']);
    assert.equal(await page.locator('#task-detail').evaluate(el => el.scrollTop), 0);
    await page.locator('#task-detail').evaluate(el => { el.scrollTop = 250; });
    await transition('return to processing detail', { selector: '[data-tab="处理中"]' }, ['processing-beta']);
    assert.equal(await page.locator('#task-detail').evaluate(el => el.scrollTop), 0);

    // Real commits must still change both the count and the selected list.
    await transition('pending before durable update', { selector: '[data-tab="待处理"]' }, ['pending-alpha']);
    await page.evaluate(async () => {
      fixture.tasks[1].status = '待处理'; fixture.tasks[1].updated++;
      fixture.core_revision = '2'; await refreshState(false);
    });
    assert.equal(await page.locator('#page-subtitle').textContent(), '2 项待处理');
    assert.deepEqual(await page.locator('.task-row').evaluateAll(rows => rows.map(row => row.dataset.task).sort()),
      ['pending-alpha', 'processing-beta']);
    await settle();

    // Updating a long list within one filter retains the current reading anchor.
    await page.evaluate(async () => {
      fixture.tasks = Array.from({ length: 35 }, (_, i) => ({ ...fixture.tasks[0],
        id: 'scroll-' + i, number: 200 + i, title: 'Synthetic scroll ' + i, updated: 1000 - i }));
      fixture.core_revision = '3'; await refreshState(false);
    });
    await settle();
    const scroll = await page.evaluate(async () => {
      const list = document.querySelector('.task-list'); list.scrollTop = 600;
      const row = [...list.children].find(node => node.getBoundingClientRect().bottom > list.getBoundingClientRect().top),
        top = row.getBoundingClientRect().top, before = list.scrollTop;
      fixture.tasks.push({ ...fixture.tasks[0], id: 'new-scroll', number: 300,
        title: 'Synthetic newly added', updated: 2000 });
      fixture.core_revision = '4'; await refreshState(false);
      await Promise.all(document.getAnimations().filter(animation => animation.effect.getTiming().iterations !== Infinity)
        .map(animation => animation.finished.catch(() => {})));
      return { retained: row.isConnected && list === document.querySelector('.task-list'),
        shift: row.getBoundingClientRect().top - top, before, after: list.scrollTop };
    });
    assert(scroll.before > 0 && scroll.retained && Math.abs(scroll.shift) < 1,
      'same-filter commits preserve the visible row and scroll anchor: ' + JSON.stringify(scroll));

    // Other reconciled lists must not leave removed content over current results either.
    const removedRows = await page.evaluate(async () => {
      const host = document.createElement('div'); document.body.append(host);
      setHtml(host, '<div class="event" data-event-key="old">Synthetic obsolete event</div>');
      setHtml(host, '<div class="event" data-event-key="new">Synthetic current event</div>');
      const samples = [];
      for (let i = 0; i < 12; i++) {
        samples.push({ text: host.textContent, ghosts: [...document.querySelectorAll('[data-motion-ghost]')]
          .some(node => node.textContent.includes('Synthetic obsolete event')) });
        await new Promise(requestAnimationFrame);
      }
      host.remove(); return samples;
    });
    assert(removedRows.every(sample => !sample.ghosts && sample.text === 'Synthetic current event'));

    await page.locator('[data-page="inbox"]').first().click();
    await page.waitForFunction(() => document.querySelector('#inbox-summary')?.textContent.startsWith('共 '));
    await settle();
    const inbox = await page.evaluate(async () => {
      const summary = document.querySelector('#inbox-summary'), text = summary.firstChild;
      let mutations = 0;
      const observer = new MutationObserver(records => { mutations += records.length; });
      observer.observe(summary, { childList: true, characterData: true, subtree: true });
      await renderInbox(); await new Promise(requestAnimationFrame);
      observer.disconnect();
      return { mutations, retained: text === summary.firstChild, opacity: Number(getComputedStyle(summary).opacity) };
    });
    assert(inbox.mutations === 0 && inbox.retained && inbox.opacity === 1,
      'unchanged inbox summary stays stable: ' + JSON.stringify(inbox));
    assert.deepEqual(errors, []);
    assert.deepEqual(failures, []);
    console.log('PASS unchanged live count has no DOM churn/fade; per-frame filter/search replacement has no stale rows/ghosts or page fade; rapid navigation and late state response; real updates, reduced motion and reading anchor preserved.');
  }).catch(error => { console.error(error); process.exitCode = 1; });
