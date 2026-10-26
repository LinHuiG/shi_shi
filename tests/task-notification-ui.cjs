const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { chromium } = require('playwright');
(async () => {
  const browser = await chromium.launch({ headless: true,
    executablePath: ['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe', 'C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync) });
  try {
    const page = await browser.newPage({ viewport: { width: 320, height: 140 }, deviceScaleFactor: 1.5 });
    const html = fs.readFileSync(path.join(__dirname, '../assets/task-notification.html'), 'utf8')
      .replace('/* THEME */', fs.readFileSync(path.join(__dirname, '../assets/ui/styles/tokens.css'), 'utf8'));
    await page.setContent(html.replace('<script>', '<script>window.sent=[];window.ipc={postMessage:value=>sent.push(value)};'));
    const actions = () => page.evaluate(() => sent.map(JSON.parse));
    await page.locator('#open').click();
    assert.deepEqual(await actions(), [{ action: 'ready', id: '' }], 'an empty card cannot navigate');
    await page.evaluate(() => setNotice({ id: 'synthetic',
      title: '<img src=x onerror="window.injected=true"> 合成事项标题', summary: '验证已通过，等待最终确认' }));
    await page.waitForFunction(() => sent.some(value => JSON.parse(value).action === 'presented'));
    assert.equal(await page.locator('main img').count(), 0);
    assert.equal(await page.locator('main p').textContent(), '验证已通过，等待最终确认');
    assert.equal(await page.locator('button').count(), 1, 'only the corner dismiss icon remains');
    assert(await page.locator('body').evaluate(el => el.scrollHeight <= el.clientHeight));
    assert.equal(await page.locator('body').evaluate(el => getComputedStyle(el).borderRadius), '8px');
    assert(await page.getByRole('button', { name: '关闭提醒' }).isVisible());
    for (const target of ['header', 'h1', 'p']) await page.locator(target).click();
    await page.locator('#open').click({ position: { x: 6, y: 130 } });
    await page.locator('#open').focus();
    await page.keyboard.press('Enter');
    await page.keyboard.press('Space');
    assert.equal((await actions()).filter(x => x.action === 'open' && x.id === 'synthetic').length, 6);
    await page.getByRole('button', { name: '关闭提醒' }).click();
    await page.keyboard.press('Escape');
    assert.equal((await actions()).filter(x => x.action === 'open').length, 6, 'dismiss must never open a task');
    assert.equal((await actions()).filter(x => x.action === 'dismiss').length, 2);
    await page.evaluate(() => setNotice({ id: 'next', title: '合成事项：核对最新变更', summary: '结果已确认，可以继续推进' }));
    await page.waitForFunction(() => sent.some(value => { const x=JSON.parse(value); return x.action==='presented' && x.id==='next'; }));
    await page.getByRole('link', { name: '查看事项：合成事项：核对最新变更', exact: true }).click();
    assert.deepEqual((await actions()).at(-1), { action: 'open', id: 'next' });
    await page.locator('#open').blur();
    await page.mouse.move(0, 0);
    const output = fs.mkdtempSync(path.join(os.tmpdir(), 'shishi-notification-'));
    await page.screenshot({ path: path.join(output, 'notification-compact.png') });
    await page.evaluate(() => setNotice({ id: 'long', title: '合成事项标题'.repeat(12), summary: '合成简短摘要'.repeat(5) }));
    const bounds = await page.evaluate(() => { const body=document.body.getBoundingClientRect(), main=document.querySelector('main').getBoundingClientRect();return { body:body.bottom, main:main.bottom, scroll:document.body.scrollHeight, height:document.body.clientHeight }; });
    assert(bounds.main <= bounds.body - 10, 'two-line title and summary must fit the smaller native window');
    assert(bounds.scroll <= bounds.height);
    await page.emulateMedia({ reducedMotion: 'reduce' });
    assert.equal(await page.locator('body').evaluate(el => getComputedStyle(el).transitionDuration), '0s');
    console.log('PASS compact task notification: full-card mouse and keyboard navigation, escaping, dismissal, replacement, long content and reduced motion; ' + output);
  } finally { await browser.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
