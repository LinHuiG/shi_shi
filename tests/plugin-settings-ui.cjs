const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { chromium } = require('playwright');

(async () => {
  const browser = await chromium.launch({ headless: true,
    executablePath: ['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe', 'C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync),
    args: ['--disable-background-networking'] });
  try {
    const page = await browser.newPage();
    await page.setContent('<div id="overlay" hidden><div id="dialog"></div></div>');
    await page.evaluate(() => {
      window.$ = selector => document.querySelector(selector);
      window.calls = [];
      window.esc = value => String(value).replace(/[&<>"']/g, '_');
      window.help = () => '';
      window.toast = () => {};
      window.refreshState = async () => {};
      window.modalOutsideDismiss = null;
      window.setModal = html => { modalOutsideDismiss = null; $('#dialog').innerHTML = html; $('#overlay').hidden = false; };
      $('#overlay').addEventListener('click', e => { if(e.target === $('#overlay') && modalOutsideDismiss?.()) closeModal(); });
      window.closeModal = () => {
        if ($('#dialog').querySelector('[data-pending="true"]')) return;
        cancelEmbeddedPluginSettings?.();
        $('#overlay').hidden = true;
      };
      window.pluginHTML = '<input id="draft" value="插件草稿"><button id="load">读取</button><button id="finish">保存配置</button><button id="cancel">取消</button><p id="output"></p><script>load.onclick=async()=>output.textContent=JSON.stringify(await shishiPlugin.call("ui_load"));finish.onclick=async()=>{try{await shishiPlugin.complete({profile:draft.value})}catch(e){output.textContent=e.message}};cancel.onclick=()=>shishiPlugin.cancel();<\/script>';
      window.rpc = async (method, value) => {
        calls.push({ method, value });
        if (method === 'source_discover') {
          if (window.delayOpen) await new Promise(resolve => { window.settleOpen = resolve; });
          return { settings: { session: 'synthetic', html: pluginHTML } };
        }
        if (method === 'pick') {
          if (window.delayPick) await new Promise(resolve => { window.settlePick = resolve; });
          return window.pickResult || null;
        }
        if (value.action === 'complete') {
          await new Promise(resolve => { window.settleSave = resolve; });
          if (window.failSave) throw Error('合成保存失败');
          window.saved = { ...value.source, config: value.config };
          return value.config;
        }
        return { loaded: true };
      };
    });
    for (const script of ['plugin-settings.js', 'extensions.js']) {
      await page.addScriptTag({ path: path.resolve(__dirname, '../assets/ui/scripts', script) });
    }
    await page.evaluate(() => {
      sourceCatalog = { plugins: [{ id: 'synthetic', name: '合成插件', settings: {}, capabilities: ['discover'], config_fields: [] }] };
      window.openEditor = () => sourceForm({ id: 'existing', plugin_id: 'synthetic', name: '来源草稿', config: { profile: 'before' }, enabled: false });
      openEditor();
    });
    const frame = page.frameLocator('iframe');
    await frame.locator('#draft').waitFor();
    assert.equal(await page.locator('iframe').getAttribute('sandbox'), 'allow-scripts');
    assert.equal(await page.locator('[data-extension="source-save"]').count(), 0);
    assert.equal(await page.getByText('返回来源配置').count(), 0);
    assert.equal(await page.locator('#source-name,#source-enabled').count(), 0);
    assert.equal(await page.evaluate(() => editingSource.name), '来源草稿');
    await frame.locator('#draft').fill('修改后的插件草稿');
    await frame.locator('#load').click();
    await frame.locator('#output').filter({ hasText: 'loaded' }).waitFor();
    await page.evaluate(() => { window.failSave = true; });
    await frame.locator('#finish').click();
    await page.waitForFunction(() => typeof settleSave === 'function');
    await page.evaluate(() => closeModal());
    assert(await page.locator('#overlay').isVisible(), 'saving cannot be mistaken for cancellation');
    assert(await page.locator('[data-source-draft-toggle]').isDisabled());
    await page.evaluate(() => { settleSave(); window.settleSave = null; });
    await frame.locator('#output').filter({ hasText: '合成保存失败' }).waitFor();
    assert.equal(await page.evaluate(() => editingSource.name), '来源草稿');
    assert.equal(await frame.locator('#draft').inputValue(), '修改后的插件草稿');
    assert(await page.locator('[data-source-draft-toggle]').isEnabled());
    await page.evaluate(() => {
      window.postMessage({ channel: 'shishi-plugin', session: 'synthetic', id: 20, action: 'complete', config: { profile: 'spoof' } }, '*');
      window.failSave = false;
    });
    await frame.locator('#finish').click();
    await page.waitForFunction(() => typeof settleSave === 'function');
    await page.evaluate(() => { settleSave(); window.settleSave = null; });
    await page.locator('#overlay').waitFor({ state: 'hidden' });
    assert.deepEqual(await page.evaluate(() => saved), { id: 'existing', plugin_id: 'synthetic', name: '来源草稿', config: { profile: '修改后的插件草稿' }, enabled: false });
    assert.equal(await page.evaluate(() => calls.some(c => c.value.config?.profile === 'spoof')), false);
    await page.evaluate(() => openEditor());
    await frame.locator('#cancel').click();
    await page.locator('#overlay').waitFor({ state: 'hidden' });
    assert.equal(await page.evaluate(() => calls.filter(c => c.value.action === 'complete').length), 2);
    const suggest = async name => {
      const child = await page.locator('iframe').elementHandle();
      return (await child.contentFrame()).evaluate(name => shishiPlugin.suggestSourceName(name), name);
    };
    for (const source of [{id:'',name:''},{id:'existing',name:'合成插件'}]) {
      await page.evaluate(source => sourceForm({ ...source, plugin_id:'synthetic',config:{},enabled:false }), source);
      await frame.locator('#draft').waitFor();
      assert.equal(await suggest('合成插件-account_one'), true);
      assert.equal(await page.evaluate(() => editingSource.name), '合成插件-account_one');
      assert.equal(await suggest('合成插件-account_two'), true, 'a later account can update the untouched default');
      await assert.rejects(suggest('x'.repeat(101)), /1–100/);
      await frame.locator('#cancel').click();
      await page.locator('#overlay').waitFor({state:'hidden'});
    }
    await page.evaluate(() => openEditor());
    await frame.locator('#draft').waitFor();
    assert.equal(await suggest('合成插件-account_one'), false, 'existing custom names remain unchanged');
    assert.equal(await page.evaluate(() => editingSource.name), '来源草稿');
    await frame.locator('#cancel').click();
    await page.locator('#overlay').waitFor({state:'hidden'});
    assert.equal(await page.evaluate(() => calls.some(c => c.value.action === 'suggest_name')), false, 'suggestions never persist or invoke a DLL');
    const reportDirty = async dirty => {
      const child = await page.locator('iframe').elementHandle();
      return (await child.contentFrame()).evaluate(dirty => shishiPlugin.setDirty(dirty), dirty);
    };
    await page.evaluate(() => openEditor());
    await frame.locator('#draft').waitFor();
    await reportDirty(true);
    await page.locator('#overlay').dispatchEvent('click');
    assert(await page.locator('#overlay').isVisible(), 'plugin edits block outside dismissal');
    await reportDirty(false);
    await page.locator('[data-source-draft-toggle]').click();
    await page.locator('#overlay').dispatchEvent('click');
    assert(await page.locator('#overlay').isVisible(), 'host fields also block outside dismissal');
    await page.locator('[data-source-draft-toggle]').click();
    await page.locator('#overlay').dispatchEvent('click');
    assert(await page.locator('#overlay').isHidden(), 'reverted edits allow outside dismissal');
    await page.evaluate(() => openEditor());
    await frame.locator('#draft').fill('older plugin edit');
    await page.waitForFunction(() => $('#source-settings-content').dataset.pluginDirty === 'true');
    await page.locator('#overlay').dispatchEvent('click');
    assert(await page.locator('#overlay').isVisible(), 'legacy plugins retain edits without explicit dirty support');
    await frame.locator('#cancel').click();
    await page.locator('#overlay').waitFor({state:'hidden'});
    await page.evaluate(() => openEditor());
    await frame.locator('#draft').waitFor();
    const child = await (await page.locator('iframe').elementHandle()).contentFrame();
    assert.equal(await child.evaluate(() => shishiPlugin.setContentHeight(180)), true);
    assert(await page.locator('#dialog').evaluate(el => el.classList.contains('plugin-settings-compact')));
    assert.equal(await child.evaluate(() => shishiPlugin.setContentHeight('bad')), false);
    await child.evaluate(() => shishiPlugin.setContentHeight(null));
    assert.equal(await page.locator('#dialog').evaluate(el => el.classList.contains('plugin-settings-compact')), false);
    await assert.rejects(child.evaluate(() => shishiPlugin.pickPath({kind:'shell'})), /文件或目录/);
    assert.equal(await child.evaluate(() => shishiPlugin.pickPath({kind:'directory',title:'合成目录'})), null);
    assert.equal(await frame.locator('#draft').inputValue(), '插件草稿');
    await page.evaluate(() => { window.delayPick = true; window.pickResult = 'D:/synthetic'; });
    const picking = child.evaluate(() => shishiPlugin.pickPath({kind:'directory',title:'合成目录',defaultPath:'D:/before'}));
    await page.waitForFunction(() => typeof settlePick === 'function');
    await page.evaluate(() => closeModal());
    assert(await page.locator('#overlay').isVisible(), 'native picker keeps the source session open');
    await page.evaluate(() => { settlePick(); window.delayPick = false; });
    assert.equal(await picking, 'D:/synthetic');
    assert.equal(await page.locator('[data-source-draft-toggle]').isEnabled(), true);
    const picked = await page.evaluate(() => calls.filter(c => c.method === 'pick').at(-1).value);
    assert.deepEqual(picked, {type:'directory',kind:'合成目录',default_path:'D:/before',filters:[]});
    await frame.locator('#cancel').click();
    await page.locator('#overlay').waitFor({state:'hidden'});
    const cancelledBeforeDelay = await page.evaluate(() => calls.filter(c => c.value.action === 'cancel').length);
    await page.evaluate(() => { window.delayOpen = true; openEditor(); });
    await page.waitForFunction(() => typeof settleOpen === 'function');
    await page.evaluate(() => { $('#overlay').inert = true; settleOpen(); });
    await page.waitForFunction(before => calls.filter(c => c.value.action === 'cancel').length === before + 1, cancelledBeforeDelay);
    assert.equal(await page.locator('iframe').count(), 0, 'late discovery must not reopen a closing editor');
    console.log('PASS source editor, isolated RPC, name suggestions/custom-name protection, one-step save, failure retry and cancellation');
  } finally { await browser.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
