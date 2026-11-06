// Offline checks of the bridge against a fake host: node --test examples/dsh-workspace-bridge/index.test.js
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, realpath, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { reconcile } from './index.js';

async function setup() {
  const root = await realpath(await mkdtemp(join(tmpdir(), 'ss-bridge-')));
  const index = join(root, 'links'), work = join(root, 'work');
  await mkdir(index);
  await mkdir(work);
  const members = [], lists = [], notices = [];
  const ctx = {
    sessionPersistence: { stat: async (id) => (id === 'unflushed' ? undefined : { header: { cwd: work } }) },
    workspaceRegistry: {
      create: async () => ({
        sessionIds: [...members],
        attachSession: async (id) => { if (id === 'broken') throw Error('attach failed'); if (!members.includes(id)) members.push(id); },
      }),
    },
    sessionController: {
      list: async () => {
        lists.push(1);
        return { items: ['old', 'a', 'b', 'c'].map((sessionId) => ({ sessionId, cwd: work, blank: false })) };
      },
    },
    emit: (name, summary) => notices.push([name, summary.sessionId, summary.cwd]),
  };
  const link = (id) => writeFile(join(index, id + '.json'), JSON.stringify({ version: 1, session_id: id }));
  return { root, index, work, ctx, members, lists, notices, link };
}

test('new sessions share one summary query and get one notice each', async () => {
  const t = await setup();
  await t.link('a');
  await t.link('b');
  const done = new Set();
  await reconcile(t.ctx, t.index, done);
  assert.equal(t.lists.length, 1);
  assert.deepEqual(t.notices, [['api-session/added', 'a', t.work], ['api-session/added', 'b', t.work]]);
  // A repeated scan, or a restart that finds them attached, notifies nothing.
  await reconcile(t.ctx, t.index, done);
  await reconcile(t.ctx, t.index, new Set());
  assert.equal(t.lists.length, 1);
  assert.equal(t.notices.length, 2);
  await rm(t.root, { recursive: true });
});

test('a batch without new sessions does not query the list', async () => {
  const t = await setup();
  t.members.push('old');
  await t.link('old');
  await t.link('unflushed');
  await reconcile(t.ctx, t.index, new Set());
  assert.equal(t.lists.length, 0);
  assert.equal(t.notices.length, 0);
  await rm(t.root, { recursive: true });
});

test('one failing session neither blocks the others nor is rolled back', async () => {
  const t = await setup();
  await t.link('a');
  await t.link('broken');
  await t.link('c');
  const done = new Set();
  await assert.rejects(reconcile(t.ctx, t.index, done), /attach failed/);
  assert.deepEqual(t.members, ['a', 'c']);
  assert.deepEqual(t.notices.map((n) => n[1]), ['a', 'c']);
  // A failing notice is reported once and the attached sessions stay attached.
  await t.link('b');
  t.ctx.emit = () => { throw Error('notice failed'); };
  await assert.rejects(reconcile(t.ctx, t.index, done), /attach failed/);
  assert.deepEqual(t.members, ['a', 'c', 'b']);
  assert.ok(done.has('b.json'));
  await rm(t.root, { recursive: true });
});
