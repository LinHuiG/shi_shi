const assert = require('node:assert/strict');
const fs = require('node:fs');
const { chromium } = require('playwright');
const bundle = require('./ui-bundle.cjs');
const now = Date.now();
const task = (id, changes = {}) => ({
  id, title: '合成事项 ' + id, description: '验证通知跳转', status: '待处理',
  tags: [{ kind: '事项等级', value: '高' }, { kind: '事项类型', value: '检查' }, { kind: '提出者', value: '甲' }],
  today: true, archived: false, deferred: false, blocker: '等待验证', due: new Date(now).toISOString(),
  created: now, updated: now, ...changes,
});
const fixture = {
  tasks: [task('target'), task('done', { status: '已完成' }), task('archived', { archived: true, today: false }),
    task('tomorrow', { deferred: true, today: false, deferred_until: now + 86400000 }),
    task('clear', { blocker: '', due: '', today: false }), task('merged', { merged_into: 'target' })],
  schedules: [], messages: [], config: { enabled: false, auto_ai: false, interval_minutes: 5, leaders: [], daily_calls: 40 },
  usage: {}, audit: [], activity: [], reports: [], suggestions: [], chat_sources: [], handling: [],
  tag_state: { unified: true, priority_kind: '事项等级', filters: {}, priorities: [], versions: [], proposals: [], groups: [
    { name: '事项等级', values: ['高', '低'] }, { name: '事项类型', values: ['检查', '计划'] },
    { name: '提出者', values: ['甲', '乙'] }, { name: '长分组', values: Array.from({ length: 35 }, (_, i) => '合成标签' + i) },
  ] },
};
const mock = `window.fixture=${JSON.stringify(fixture)};window.requests=[];window.ipc={postMessage(raw){
  const {id,method,params:p}=JSON.parse(raw);requests.push({method,p});
  setTimeout(()=>{let data=true;if(method==='state'){
    if(window.failState){receive({id,ok:false,error:'synthetic unavailable'});return;}
    data={...structuredClone(fixture),_page:p.page};
  }if(method==='evidence')data=[];if(method==='tag_filters'){fixture.tag_state.filters=p.filters;fixture.tag_state.priorities=p.priorities;data=p.filters;}
  if(method==='plugins')data={plugins:[],sources:[],errors:[]};
  receive({id,ok:true,data});},method==='state'?(window.stateDelay?.[p.task]||0):0);
}};`;
(async () => {
  const browser = await chromium.launch({ headless: true,
    executablePath: process.env.SHISHI_UI_BROWSER || ['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe', 'C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync),
    args: ['--disable-background-networking'] });
  try {
    const page = await browser.newPage({ viewport: { width: 1500, height: 900 }, reducedMotion: 'reduce' });
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.setContent(bundle().replace('<script>', '<script>' + mock));
    await page.locator('.task-row').first().waitFor();
    const configure = value => page.evaluate(async value => {
      await tagSave; closeModal();
      page = 'tasks'; state._page = 'tasks'; tab = value.tab || '全部'; quickView = value.quickView || '';
      query = value.query || ''; selectedTags = value.tags || {}; selectedPriorities = value.priorities || [];
      render(); requests.length = 0;
    }, value);
    const filters = () => page.evaluate(() => ({ tab, quickView, query, tags: tagSelection(), priorities: prioritySelection() }));
    const open = id => page.evaluate(id => openNotifiedTask(id), id);
    const selected = id => page.locator(`.task-row[data-task="${id}"].selected`).count();

    const matching = { tab: '待处理', quickView: 'blocked', query: '合成事项',
      tags: { 事项等级: ['高', '低'], 事项类型: ['检查'], 提出者: ['甲'] }, priorities: ['高'] };
    await configure(matching);
    assert.equal(await open('target'), true);
    assert.deepEqual(await filters(), matching);
    assert.equal(await selected('target'), 1);
    assert.equal(await page.evaluate(() => requests.filter(r => r.method === 'tag_filters').length), 0);

    await configure({ ...matching, tags: { ...matching.tags, 事项类型: ['计划'] }, priorities: ['低'] });
    assert.equal(await open('target'), true);
    assert.deepEqual(await filters(), { ...matching, tags: { 事项等级: ['高', '低'], 提出者: ['甲'] }, priorities: [] });
    await page.evaluate(() => tagSave);
    assert.equal(await page.evaluate(() => requests.filter(r => r.method === 'tag_filters').length), 1);

    for (const [id, scope] of [['done', '全部'], ['archived', '归档'], ['tomorrow', '全部']]) {
      await configure({ tab: '待处理', tags: { 提出者: ['甲'] }, query: '合成事项' });
      const before = await page.evaluate(() => JSON.stringify(fixture.tasks));
      assert.equal(await open(id), true, id);
      assert.equal(await selected(id), 1);
      assert.deepEqual(await filters(), { tab: scope, quickView: '', query: '合成事项', tags: { 提出者: ['甲'] }, priorities: [] });
      assert.equal(await page.evaluate(() => JSON.stringify(fixture.tasks)), before, 'navigation must not change business data');
    }
    await configure({ tab: '待处理', quickView: 'today', query: 'target', tags: { 事项类型: ['检查'] } });
    assert.equal(await open('target'), true);
    assert.equal((await filters()).quickView, 'today');
    assert.equal(await open('clear'), true);
    assert.deepEqual(await filters(), { tab: '待处理', quickView: '', query: '', tags: { 事项类型: ['检查'] }, priorities: [] });

    for (const id of ['deleted', 'merged']) {
      const before = await filters();
      assert.equal(await open(id), false);
      assert.deepEqual(await filters(), before);
    }
    await page.evaluate(() => { window.failState = true; });
    assert.equal(await open('target'), false);
    await page.evaluate(() => { window.failState = false; });

    await page.evaluate(() => formModal(state.tasks.find(t => t.id === 'target')));
    await page.locator('#f-title').fill('尚未保存的标题');
    assert.equal(await open('done'), false);
    assert.equal(await page.locator('#f-title').inputValue(), '尚未保存的标题');
    await page.evaluate(() => { closeModal(); formModal(state.tasks.find(t => t.id === 'target')); });
    assert.equal(await open('done'), true, 'unchanged task form can close');

    await page.evaluate(() => {
      setModal('<input id="source-field-file" value="合成来源"><button data-source-draft-toggle aria-checked="false" data-initial-enabled="false"></button><div id="source-settings-content"></div>');
      allowUnchangedSourceDismissal();
    });
    await page.locator('#source-field-file').fill('未保存的来源');
    assert.equal(await open('target'), false);
    assert.equal(await page.locator('#source-field-file').inputValue(), '未保存的来源');
    await page.locator('#source-field-file').fill('合成来源');
    await page.locator('#source-settings-content').evaluate(el => { el.dataset.pluginDirty = 'true'; });
    assert.equal(await open('target'), false);
    await page.locator('#source-settings-content').evaluate(el => { el.dataset.pluginDirty = 'false'; el.dataset.pending = 'true'; });
    assert.equal(await open('target'), false);
    await page.locator('#source-settings-content').evaluate(el => { delete el.dataset.pending; });
    assert.equal(await open('target'), true);

    assert.equal(await page.evaluate(async () => {
      const leave = leaveSettings; leaveSettings = async () => false;
      try { return await openNotifiedTask('target'); } finally { leaveSettings = leave; }
    }), false);
    await page.evaluate(() => { window.stateDelay = { target: 120 }; window.navigation = openNotifiedTask('target'); });
    await page.evaluate(() => { formModal(state.tasks[0]); document.querySelector('#f-title').value = '请求中开始编辑'; });
    assert.equal(await page.evaluate(() => navigation), false);
    assert.equal(await page.locator('#f-title').inputValue(), '请求中开始编辑');
    await page.evaluate(() => { closeModal(); });
    assert.deepEqual(await page.evaluate(() => Promise.all([openNotifiedTask('target'), openNotifiedTask('done')])), [false, true]);
    assert.equal(await selected('done'), 1);
    await page.evaluate(() => { window.stateDelay = {}; });

    // Compact groups use content width; fading follows actual remaining overflow.
    await configure({});
    const group = page.locator('[data-filter-group="tag:提出者"]');
    const shortWidth = (await group.boundingBox()).width;
    await page.setViewportSize({ width: 1900, height: 900 });
    assert(Math.abs((await group.boundingBox()).width - shortWidth) < 1, 'short group must not stretch');
    assert(shortWidth < (await page.locator('[data-filter-group="view"]').boundingBox()).width);
    assert.equal(await group.locator('.tag-options').evaluate(el => getComputedStyle(el).maskImage), 'none');
    const overflow = page.locator('[data-filter-group="tag:长分组"] .tag-options');
    await page.waitForFunction(() => document.querySelector('[data-filter-group="tag:长分组"] .has-more-options'));
    assert.notEqual(await overflow.evaluate(el => getComputedStyle(el).maskImage), 'none');
    await overflow.evaluate(el => { el.scrollLeft = el.scrollWidth; });
    await page.waitForFunction(() => !document.querySelector('[data-filter-group="tag:长分组"] .has-more-options'));
    assert.equal(await overflow.evaluate(el => getComputedStyle(el).maskImage), 'none');
    const before = await page.locator('.tag-slot').evaluateAll(els => els.map(el => { const r = el.getBoundingClientRect(); return [r.x, r.y, r.width, r.height]; }));
    await page.locator('[data-tag-expand="长分组"]').click();
    assert.deepEqual(await page.locator('.tag-slot').evaluateAll(els => els.map(el => { const r = el.getBoundingClientRect(); return [r.x, r.y, r.width, r.height]; })), before);
    assert.equal(await overflow.evaluate(el => getComputedStyle(el).maskImage), 'none');
    await page.locator('#page-title').click();
    await page.setViewportSize({ width: 840, height: 720 });
    assert(await page.locator('.tag-groups').evaluate(el => el.scrollWidth <= el.clientWidth + 1));
    assert.deepEqual(errors, []);
    console.log('PASS notification navigation: selective filters, archived/deferred tasks, drafts, failed/stale requests; content-sized tags and real-overflow masks');
  } finally { await browser.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
