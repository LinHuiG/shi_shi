// Offline UI checks with synthetic RPC replies; no desktop clients are opened.
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const test = require('node:test');

const root = join(__dirname, '..');
function load(context, ...scripts) {
  for (const name of scripts)
    vm.runInContext(readFileSync(join(root, 'assets/ui/scripts', name + '.js'), 'utf8'), context);
}

function handlingFixture() {
  const elements = new Map();
  for (const selector of ['#handling-project', '#handling-project-note', '#handling-start-note', '#dialog [data-handle="submit"]', '#handling-instructions'])
    elements.set(selector, { value: '合成补充说明', classList: { toggle() {} } });
  const listeners = [];
  const context = vm.createContext({
    document: { addEventListener(type, handler) { listeners.push([type, handler]); } },
    addEventListener() {},
    $: (selector) => elements.get(selector), esc: String, icon: () => '',
    effortNames: { high: '高' }, state: { handling: [] },
    setModal(html) { context.modal = html; },
    connections: [
      { id: 'codex', driver: 'codex', name: 'Codex', models: [] },
      { id: 'dsh', driver: 'dsh', name: 'DSH', models: [{ model: 'm', supportedReasoningEfforts: [{ reasoningEffort: 'high' }] }] },
    ],
    projects: [],
    async rpc(method, params) {
      if (method === 'handling_prepare') return { connections: context.connections, prompt: '合成提示词', suggested: '' };
      assert.equal(method, 'handling_projects');
      context.projects.push(params.connection_id);
      return [{ id: params.connection_id + ':p', name: '合成项目', cwd: 'C:/synthetic' }];
    },
  });
  load(context, 'handling');
  context.click = listeners.find(([type]) => type === 'click')[1];
  context.choose = (connection, model = '') => {
    vm.runInContext(`handlingMenu = { kind: 'model', host: { firstElementChild: {} }, input: { value: '' }, el: { querySelector() { return {}; } } }`, context);
    context.click({ target: { closest(selector) {
      return selector === '.handling-menu button' ? { dataset: { connection, model } } : null;
    } } });
  };
  return context;
}

test('client preference stays in memory and only selects an available client', async () => {
  const c = handlingFixture();
  await vm.runInContext('beginHandling("task")', c);
  assert.equal(c.projects.at(-1), 'codex');
  assert.match(vm.runInContext('handlingMenuItems("model", null, "")', c), /data-connection="dsh"/);
  c.choose('dsh', 'm');
  await vm.runInContext('beginHandling("task")', c);
  assert.equal(c.projects.at(-1), 'dsh');
  assert.equal(vm.runInContext('handlingChoices.pick.model', c), '', 'only the client is remembered');
  assert.match(c.modal, /DSH/);
  c.connections = c.connections.filter((connection) => connection.driver === 'codex');
  await vm.runInContext('beginHandling("task")', c);
  assert.equal(c.projects.at(-1), 'codex');
  assert.doesNotMatch(vm.runInContext('handlingMenuItems("model", null, "")', c), /DSH|dsh/);
  c.connections = [];
  const count = c.projects.length;
  await assert.rejects(vm.runInContext('beginHandling("task")', c), /请先在设置中添加/);
  assert.equal(c.projects.length, count);
  const reopened = handlingFixture();
  await vm.runInContext('beginHandling("task")', reopened);
  assert.equal(reopened.projects.at(-1), 'codex');
});

test('switching clients refreshes projects and preserves the instruction draft', async () => {
  const c = handlingFixture();
  await vm.runInContext('beginHandling("task")', c);
  c.choose('dsh', 'm');
  await new Promise(setImmediate);
  assert.equal(vm.runInContext('handlingChoices.project', c), 'dsh:p');
  assert.equal(vm.runInContext('$("#handling-instructions").value', c), '合成补充说明');
  assert.match(vm.runInContext('handlingChip(handlingChoices.pick)', c), /DSH/);
  assert.match(vm.runInContext('handlingMenuItems("model", null, "")', c), /推理强度/);
  c.choose('codex');
  await new Promise(setImmediate);
  assert.equal(vm.runInContext('handlingChoices.project', c), 'codex:p');
  assert.doesNotMatch(vm.runInContext('handlingChip(handlingChoices.pick)', c), /默认模型|默认强度/);
  assert.doesNotMatch(vm.runInContext('handlingMenuItems("model", null, "")', c), /推理强度/);
});

test('settings embeds system prompts before storage and omits the handling model route', async () => {
  const c = vm.createContext({
    document: { addEventListener() {} }, page: 'settings', esc: String, effortNames: {},
    state: { config: { codex: '', ai_connections: [], model_profiles: {}, leaders: [] } },
    closeConnectionEditor() {}, tagPriorityFields: () => '', retentionSummary: () => '', aboutSection: () => '',
    readSettingsValues: () => ({}), syncUpdateControls() {},
    renderSources() {}, renderSystemPrompts: async () => {},
    setHtml(selector, html) { assert.equal(selector, '#view'); c.html = html; },
  });
  load(c, 'settings', 'model-routes');
  vm.runInContext('loadModels = () => {}; syncAutomationControls = () => {}', c);
  await vm.runInContext('renderSettings()', c);
  const settings = c.html;
  assert.ok(settings.indexOf('data-settings-target="system-prompts"') < settings.indexOf('data-settings-target="storage"'));
  assert.ok(settings.indexOf('id="settings-system-prompts"') < settings.indexOf('id="settings-storage"'));
  assert.match(settings, /id="settings-prompt-content" data-preserve-content/);
  assert.doesNotMatch(settings, /data-page="prompts"/);
  assert.match(settings, /data-model-profile="analysis"/);
  assert.match(settings, /data-model-profile="review"/);
  assert.match(settings, /data-model-profile="filter"/);
  assert.doesNotMatch(settings, /cfg-handling|data-model-profile="handling"/);
});

test('tool library renders a page from the state snapshot and filters without a request', () => {
  const count = {}, html = new Map();
  const c = vm.createContext({
    document: { addEventListener() {} }, esc: String,
    $: (selector) => { assert.equal(selector, '#tool-count'); return count; },
    setHtml: (selector, content) => html.set(selector, content),
    empty: (title, message) => `${title} ${message}`,
    state: { config: { tools: [
      { id: 'a', name: 'query_items', display_name: '查询事项', description: '按关键字查询合成事项', enabled: true, parameters: [{ name: 'keyword' }] },
      { id: 'b', name: 'read_notes', display_name: '读取笔记', description: '读取合成笔记', enabled: false, parameters: [] },
    ] } },
  });
  load(c, 'tools');
  vm.runInContext('showToolLibrary()', c);
  assert.match(html.get('#view'), /class="page tool-library"/);
  assert.doesNotMatch(html.get('#view'), /dialog-foot|data-action="close"/);
  assert.match(html.get('#tool-list-rows'), /data-tool-edit="a"/);
  assert.match(html.get('#tool-list-rows'), /aria-checked="true"/);
  assert.equal(count.textContent, '2 / 2个工具');
  vm.runInContext('toolQuery = "笔记"; renderToolRows()', c);
  assert.doesNotMatch(html.get('#tool-list-rows'), /data-tool-edit="a"/);
  assert.match(html.get('#tool-list-rows'), /data-tool-edit="b"/);
  assert.equal(count.textContent, '1 / 2个工具');
  vm.runInContext('toolQuery = "不存在"; renderToolRows()', c);
  assert.match(html.get('#tool-list-rows'), /没有匹配的工具/);
  c.state.config.tools = [];
  vm.runInContext('renderToolRows()', c);
  assert.match(html.get('#tool-list-rows'), /还没有工具/);
});
