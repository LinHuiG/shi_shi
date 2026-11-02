// A direct suite owns its browser; the runner shares one process, with a fresh
// connection and isolated contexts per suite. Closing a connection cannot close
// the runner's browser or another suite's contexts.
const fs = require('node:fs');
const {chromium: engine} = require('playwright');
const launchOptions = () => ({
  headless: true,
  executablePath: process.env.SHISHI_UI_BROWSER || [
    'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
    'C:/Program Files/Microsoft/Edge/Application/msedge.exe',
  ].find(fs.existsSync),
  args: ['--disable-background-networking'],
});
const chromium = {
  launch: () => process.env.SHISHI_UI_BROWSER_WS
    ? engine.connect(process.env.SHISHI_UI_BROWSER_WS)
    : engine.launch(launchOptions()),
};
async function withBrowser(run) {
  const browser = await chromium.launch();
  try {
    return await run(browser);
  } finally {
    await browser.close();
  }
}
module.exports = {
  chromium,
  withBrowser,
  // All suites connect through the same explicit loopback interface.
  launchServer: () => engine.launchServer({...launchOptions(), host: '127.0.0.1'}),
};
