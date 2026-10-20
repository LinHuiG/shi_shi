// ACP owns stdout. Redirect console diagnostics before any DSH plugin loads;
// protocol writers using process.stdout remain untouched.
const { Console } = require('node:console');
globalThis.console = new Console({ stdout: process.stderr, stderr: process.stderr });
// Keep the guard local to this DSH process. Node-based project tools must keep
// their ordinary stdout behavior and the user's original NODE_OPTIONS.
if (Object.hasOwn(process.env, 'SHISHI_DSH_ORIGINAL_NODE_OPTIONS')) {
  const original = process.env.SHISHI_DSH_ORIGINAL_NODE_OPTIONS;
  if (original) process.env.NODE_OPTIONS = original;
  else delete process.env.NODE_OPTIONS;
  delete process.env.SHISHI_DSH_ORIGINAL_NODE_OPTIONS;
}
