import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';
import { Readable } from 'node:stream';
import * as plugin from '../src/index.mjs';

test('real upstream Cordis publishes selected visible content and bounded snapshots', { skip: !process.env.NEXUS_TEST_DSH_ROOT }, async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nexus-notifications-test-'));
  const oldFile = process.env.NEXUS_NOTIFICATION_FILE, oldRun = process.env.NEXUS_NOTIFICATION_RUN;
  const file = path.join(root, 'events.json');
  process.env.NEXUS_NOTIFICATION_FILE = file; process.env.NEXUS_NOTIFICATION_RUN = 'isolated-test-run';
  let fork;
  try {
    const require = createRequire(path.join(process.env.NEXUS_TEST_DSH_ROOT, 'apps/cli/package.json'));
    const { Context } = await import(pathToFileURL(require.resolve('@deepseek-ai/cordis')).href);
    const ctx = new Context(); ctx.provide('sessions', {});
    let route;
    ctx.provide('webServer', { register: value => { route = value; return () => { route = undefined; }; } });
    fork = ctx.plugin(plugin);
    await new Promise(resolve => setTimeout(resolve, 100));
    assert.equal(route.path, '/nexus-notifications/view');
    const req = Readable.from([JSON.stringify({ page: 'test-page', session: 'test', focused: true })]);
    req.method = 'POST'; req.headers = { origin: 'http://127.0.0.1:1234', host: '127.0.0.1:1234', 'content-type': 'application/json', 'x-nexus-view': '1' };
    await route.handler(req, { writeHead: status => assert.equal(status, 204), end() {} });
    const session = { header: { id: 'test', origin: 'user' } };
    ctx.emit('session/event', session, { type: 'session/title', data: { title: '通知验收' } });
    ctx.emit('session/event', session, { type: 'turn/start', data: { turn: 1 } });
    ctx.emit('session/event', session, { type: 'tool/call', data: { callId: 'q', name: 'ask_user_question', arguments: JSON.stringify({ questions: [{ question: '发布哪个版本？' }] }) } });
    await new Promise(resolve => setTimeout(resolve, 1100));
    ctx.emit('session/event', session, { type: 'tool/result', data: { callId: 'q', content: 'PRIVATE ANSWER' } });
    ctx.emit('session/event', session, { type: 'assistant/message', data: { turn: 1, message: { content: [{ type: 'text', text: '已完成发布准备。' }, { type: 'reasoning', text: 'PRIVATE THOUGHT' }] } } });
    ctx.emit('session/event', session, { type: 'turn/end', data: { turn: 1, reason: { kind: 'completed' } } });
    const data = JSON.parse(fs.readFileSync(file, 'utf8'));
    assert.ok(data.capabilities.includes('sessions'));
    assert.deepEqual(data.events.map(e => e.kind), ['question', 'completed']);
    assert.equal(JSON.stringify(data).includes('PRIVATE'), false);
    assert.equal(data.events[0].body, '发布哪个版本？');
    assert.equal(data.events[1].title, '通知验收'); assert.equal(data.events[1].body, '已完成发布准备。');
    assert.equal(data.run, 'isolated-test-run');
    assert.equal(data.views[0].session, 'test'); assert.equal(data.views[0].focused, true);
    for (let turn = 2; turn < 140; turn++) {
      ctx.emit('session/event', session, { type: 'turn/start', data: { turn } });
      ctx.emit('session/event', session, { type: 'assistant/message', data: { turn, message: { content: [{ type: 'text', text: '😀'.repeat(800) }] } } });
      ctx.emit('session/event', session, { type: 'turn/end', data: { turn, reason: { kind: 'completed' } } });
    }
    assert.ok(fs.statSync(file).size <= 240000);
    const final = JSON.parse(fs.readFileSync(file)); assert.equal(final.events.at(-1).sequence, 140);
  } finally {
    await fork?.dispose();
    if (oldFile === undefined) delete process.env.NEXUS_NOTIFICATION_FILE; else process.env.NEXUS_NOTIFICATION_FILE = oldFile;
    if (oldRun === undefined) delete process.env.NEXUS_NOTIFICATION_RUN; else process.env.NEXUS_NOTIFICATION_RUN = oldRun;
    assert.equal(path.dirname(root), os.tmpdir());
    fs.rmSync(root, { recursive: true, force: true });
  }
});
