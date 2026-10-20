// No custom chat UI or transcript copying: attach persisted SS sessions to the
// desktop's native workspace registry. The desktop remains a read-only viewer
// while the ACP process owns the session writer.
import { readdir, readFile, realpath, stat, writeFile, rename } from 'node:fs/promises';
import { join, isAbsolute } from 'node:path';

export const name = 'shishi-workspace-bridge';
export const inject = ['workspaceRegistry', 'sessionPersistence'];

export async function reconcile(ctx, indexDir, done = new Set()) {
  let files;
  try { files = await readdir(indexDir); }
  catch (error) { if (error.code === 'ENOENT') return; throw error; }
  let processed = 0;
  for (const file of files.sort()) {
    if (!/^[a-zA-Z0-9_-]{1,128}\.json$/.test(file) || done.has(file)) continue;
    if (++processed > 100) break;
    const path = join(indexDir, file);
    if ((await stat(path)).size > 4096) throw Error('SS session link is too large');
    const link = JSON.parse(await readFile(path, 'utf8'));
    if (link.version !== 1 || link.session_id + '.json' !== file) throw Error('Invalid SS session link');
    // The native persisted header is authoritative, never the manifest's cwd.
    const snapshot = await ctx.sessionPersistence.stat(link.session_id);
    if (!snapshot) continue; // ACP has not flushed its first record yet.
    const cwd = snapshot.header.cwd;
    if (!cwd || !isAbsolute(cwd)) throw Error('SS session has no absolute workspace');
    const canonical = await realpath(cwd);
    if (!(await stat(canonical)).isDirectory()) throw Error('SS workspace is unavailable');
    const project = await ctx.workspaceRegistry.create(canonical);
    await project.attachSession(link.session_id);
    done.add(file);
  }
}

export function apply(ctx, config) {
  if (!config?.indexDir || !isAbsolute(config.indexDir)) throw Error('SS bridge requires an absolute indexDir');
  const done = new Set();
  let closed = false, pending = Promise.resolve(), previousError = '';
  const tick = () => {
    pending = pending.then(async () => {
      if (closed) return;
      try {
        await reconcile(ctx, config.indexDir, done);
        const projects=ctx.workspaceRegistry.list().map(p=>({id:'dsh:'+p.id,name:p.title,cwd:p.path}));
        const temp=join(config.indexDir,'.workspaces.tmp');
        await writeFile(temp,JSON.stringify({version:1,projects}));
        await rename(temp,join(config.indexDir,'.workspaces.json'));
        previousError = '';
      }
      catch (error) {
        const message = String(error);
        if (message !== previousError) ctx.logger.warn('SS workspace bridge: ' + message);
        previousError = message;
      }
    });
  };
  tick();
  const timer = setInterval(tick, 3000);
  timer.unref?.();
  ctx.effect(() => async () => { closed = true; clearInterval(timer); await pending; });
}
