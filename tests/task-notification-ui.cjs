const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { chromium } = require('playwright');
(async () => {
  const browser = await chromium.launch({ headless: true,
    executablePath: ['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe', 'C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync) });
  try {
    const page = await browser.newPage({ viewport: { width: 350, height: 178 } });
    const html = fs.readFileSync(path.join(__dirname, '../assets/task-notification.html'), 'utf8')
      .replace('/* THEME */', fs.readFileSync(path.join(__dirname, '../assets/ui/styles/tokens.css'), 'utf8'));
    await page.setContent(html.replace('<script>', '<script>window.sent=[];window.ipc={postMessage:value=>sent.push(value)};'));
    await page.evaluate(() => setNotice({ id: 'synthetic',
      title: '<img src=x onerror="window.injected=true"> 合成事项标题', summary: '验证已通过，等待最终确认' }));
    await page.waitForFunction(() => sent.some(value => JSON.parse(value).action === 'presented'));
    assert.equal(await page.locator('main img').count(), 0);
    assert.equal(await page.locator('h1').count(), 1);
    assert.equal(await page.locator('main p').textContent(), '验证已通过，等待最终确认');
    assert.equal(await page.locator('main ul,main li').count(), 0);
    assert(await page.locator('body').evaluate(el => el.scrollHeight <= el.clientHeight));
    assert(await page.locator('main').evaluate(el => el.scrollHeight <= el.clientHeight));
    assert.equal(await page.locator('body').evaluate(el => getComputedStyle(el).borderRadius), '8px');
    assert.equal(await page.locator('body').evaluate(el => getComputedStyle(el).backgroundColor), 'rgb(32, 32, 32)');
    assert(await page.getByRole('button', { name: '关闭提醒' }).isVisible());
    await page.getByRole('button', { name: '查看事项' }).click();
    const output = fs.mkdtempSync(path.join(os.tmpdir(), 'shishi-notification-'));
    await page.screenshot({ path: path.join(output, 'notification.png') });
    await page.getByRole('button', { name: '关闭提醒' }).click();
    await page.keyboard.press('Escape');
    assert.deepEqual(await page.evaluate(() => sent.map(JSON.parse)), [
      { action: 'ready', id: '' }, { action: 'presented', id: 'synthetic' },
      { action: 'open', id: 'synthetic' }, { action: 'dismiss', id: 'synthetic' }, { action: 'dismiss', id: 'synthetic' },
    ]);
    await page.evaluate(() => setNotice({ id: 'next', title: '合成事项：核对最新变更', summary: '结果已确认，可以继续推进' }));
    await page.getByRole('button', { name: '关闭提醒' }).blur();
    await page.screenshot({ path: path.join(output, 'notification-compact.png') });
    await page.evaluate(() => setNotice({ id: 'long', title: '合成事项标题'.repeat(12), summary: '合成简短摘要'.repeat(5) }));
    assert(await page.locator('main').evaluate(el => el.scrollHeight <= el.clientHeight), 'two-line title and maximum summary must fit');
    console.log('PASS task notification: one title and summary, compact dark layout, escaping, task-bound open and dismissal; ' + output);
  } finally { await browser.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
