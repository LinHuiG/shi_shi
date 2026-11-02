// Keep each suite in its own Node process and browser contexts. A single browser
// startup is enough, while assertions, clocks and globals remain isolated.
const fs = require('node:fs');
const path = require('node:path');
const {spawn} = require('node:child_process');
const suites = fs.readdirSync(__dirname).filter(name => name.endsWith('-ui.cjs') || name === 'ui-regression.cjs')
  .filter(name => name !== 'run-ui.cjs').sort();
const args = process.argv.slice(2), list = args.includes('--list');
const names = args.filter(arg => arg !== '--list').map(arg => path.basename(arg));
if (names.some(name => !suites.includes(name))) throw new Error('Unknown UI suite: ' + names.filter(name => !suites.includes(name)).join(', '));
const selected = names.length ? [...new Set(names)] : suites;
if (list) { console.log(selected.join('\n')); } else {
  (async () => {
    const browser = await require('./ui-driver.cjs').launchServer();
    const started = Date.now(), results = [], children = new Set();
    let cancelled = false;
    const cancel = () => { cancelled = true; for (const child of children) child.kill(); };
    process.once('SIGINT', cancel); process.once('SIGTERM', cancel);
    try {
      // Sequential assertions keep animation/frame tests independent of CPU
      // contention. Sharing startup removes overhead without racing fixtures.
      for (const name of selected) {
        if (cancelled) break;
        const start = Date.now();
        const result = await new Promise(resolve => {
          const child = spawn(process.execPath, [path.join(__dirname, name)], {
            stdio: ['ignore', 'pipe', 'pipe'], env: {...process.env, SHISHI_UI_BROWSER_WS: browser.wsEndpoint()},
          });
          children.add(child); let output = '';
          child.stdout.on('data', data => { output += data; }); child.stderr.on('data', data => { output += data; });
          // A wedged suite must fail visibly instead of blocking the full run.
          const timer = setTimeout(() => child.kill(), 180000);
          child.on('error', error => { output += error.message; });
          child.on('close', code => { clearTimeout(timer); children.delete(child); resolve({name, ok: code === 0, seconds: (Date.now() - start) / 1000, output}); });
        });
        results.push(result);
        console.log(`${result.ok ? 'PASS' : 'FAIL'} ${name} ${result.seconds.toFixed(2)}s`);
        if (!result.ok) console.error(result.output);
      }
    } finally {
      for (const child of children) child.kill();
      await browser.close();
      process.removeListener('SIGINT', cancel); process.removeListener('SIGTERM', cancel);
    }
    const directory = path.join(__dirname, '..', 'build'); fs.mkdirSync(directory, {recursive: true});
    fs.writeFileSync(path.join(directory, 'ui-results.json'), JSON.stringify({seconds: (Date.now() - started) / 1000, results}, null, 2));
    console.log(`${results.filter(r => r.ok).length}/${results.length} UI suites passed in ${((Date.now() - started) / 1000).toFixed(1)}s`);
    if (cancelled || results.some(r => !r.ok)) process.exitCode = 1;
  })().catch(error => {console.error(error); process.exitCode = 1;});
}
