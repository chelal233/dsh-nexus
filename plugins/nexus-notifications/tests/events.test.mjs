import test from 'node:test';
import assert from 'node:assert/strict';
import { createTracker, preview, viewHandler } from '../src/index.mjs';
import { Readable } from 'node:stream';

const session = { header: { id: 's1', origin: 'user' } };
test('answering a question does not consume the final completion, including a resumed turn', () => {
  const events = [], timers = new Map(); let id = 0;
  const tracker = createTracker(e => events.push(e), fn => { timers.set(++id, fn); return id; }, id => timers.delete(id));
  const send = (type, data) => tracker.event(session, { type, data });
  send('turn/start', { turn: 1 });
  send('tool/call', { callId: 'q', name: 'ask_user_question' });
  for (const fn of timers.values()) fn(); timers.clear();
  send('tool/result', { callId: 'q' });
  send('turn/end', { turn: 1, reason: { kind: 'completed' } });
  send('turn/end', { turn: 1, reason: { kind: 'completed' } });
  send('turn/start', { turn: 2 }); // No new user/message required after resume.
  send('turn/end', { turn: 2, reason: { kind: 'completed' } });
  assert.deepEqual(events.map(e => e.kind), ['question', 'completed', 'completed']);
});
test('automatic approval, cancellation, history and subagents do not create false completion notices', () => {
  const events = [], timers = new Map(); let id = 0;
  const tracker = createTracker(e => events.push(e), fn => { timers.set(++id, fn); return id; }, id => timers.delete(id));
  tracker.event(session, { type: 'turn/start', data: { turn: 1 } });
  tracker.event(session, { type: 'approval/asked', data: { id: 'a' } });
  tracker.event(session, { type: 'approval/decided', data: { id: 'a' } });
  for (const fn of timers.values()) fn();
  tracker.event(session, { type: 'turn/end', data: { turn: 1, reason: { kind: 'aborted' } } });
  tracker.event(session, { type: 'turn/end', data: { turn: 9, reason: { kind: 'completed' } } });
  tracker.event({ header: { id: 'child', origin: 'subagent' } }, { type: 'turn/start', data: { turn: 1 } });
  tracker.event({ header: { id: 'child', origin: 'subagent' } }, { type: 'turn/end', data: { turn: 1, reason: { kind: 'completed' } } });
  assert.deepEqual(events, []);
});
test('failure, blocked, and job outcomes are distinct and job duplicate delivery is ignored', () => {
  const events = [], tracker = createTracker(e => events.push(e));
  for (const [turn, kind] of [[1,'error'],[2,'max-tokens'],[3,'blocked']]) {
    tracker.event(session, { type: 'turn/start', data: { turn } });
    tracker.event(session, { type: 'turn/end', data: { turn, reason: { kind } } });
  }
  tracker.job({ id: 'j', status: 'completed' }); tracker.job({ id: 'j', status: 'completed' });
  tracker.job({ id: 'k', status: 'failed' });
  assert.deepEqual(events.map(e => e.kind), ['failed','failed','blocked','job-completed','job-failed']);
});

test('conversation previews are isolated by session and turn and omit reasoning/tool output', () => {
  const events = [], tracker = createTracker(e => events.push(e));
  const send = (type, data, s = session) => tracker.event(s, { type, data });
  send('session/title', { title: '修复通知' });
  send('turn/start', { turn: 1 });
  send('assistant/message', { turn: 1, message: { content: [{ type: 'text', text: '修改完成，测试通过。' }, { type: 'reasoning', text: 'PRIVATE' }] } });
  send('tool/result', { callId: 'x', content: 'PRIVATE tool result' });
  send('turn/start', { turn: 1 }, { header: { id: 'other' } });
  send('assistant/message', { turn: 1, message: { content: [{ type: 'text', text: 'OTHER' }] } }, { header: { id: 'other' } });
  send('turn/end', { turn: 1, reason: { kind: 'completed' } });
  send('turn/start', { turn: 2 });
  send('turn/end', { turn: 2, reason: { kind: 'completed' } });
  assert.equal(events[0].title, '修复通知'); assert.equal(events[0].body, '修改完成，测试通过。');
  assert.equal(events[1].body, ''); assert.doesNotMatch(JSON.stringify(events), /PRIVATE|OTHER/);
  assert.equal([...preview('中'.repeat(900))].length, 500);
  assert.equal(preview('ok\x1b]9;evil\x07\x1b[31m\x07done'), 'ok done');
});

test('questions, approvals, failures and jobs carry actual context, not opaque objects', () => {
  const events = [], timers = [];
  const tracker = createTracker(e => events.push(e), fn => { timers.push(fn); }, () => {}, () => '已有对话');
  const send = (type, data) => tracker.event(session, { type, data });
  send('turn/start', { turn: 1 });
  send('tool/call', { callId: 'q', name: 'ask_user_question', arguments: JSON.stringify({ questions: [{ question: '部署哪个环境？' }, { question: '使用哪个版本？' }] }) });
  send('approval/asked', { id: 'a', toolName: 'bash', reason: '需要运行构建命令' });
  timers.forEach(fn => fn());
  send('turn/end', { turn: 1, reason: { kind: 'error', error: { message: '请求超时', code: 'TIMEOUT' } } });
  tracker.job({ id: 'j', ownerSession: 's1', status: 'failed', label: '构建项目', detail: 'exit code: 2' }, session);
  assert.deepEqual(events.map(e => e.body), ['部署哪个环境？ / 使用哪个版本？', 'bash — 需要运行构建命令', '请求超时 — TIMEOUT', '构建项目 — exit code: 2']);
  assert.ok(events.every(e => e.title === '已有对话' && e.session === 's1'));
});

test('page presence requires same origin, stays bounded, and clears on blur or expiry', async () => {
  const views = new Map(); let time = 10000, changed = 0;
  const handler = viewHandler(views, () => changed++, () => time);
  const post = async (value, origin = 'http://127.0.0.1:1234') => {
    const req = Readable.from([JSON.stringify(value)]);
    req.method = 'POST'; req.headers = { origin, host: '127.0.0.1:1234', 'x-nexus-view': '1', 'content-type': 'application/json' };
    let status; await handler(req, { writeHead: s => { status = s; }, end() {} }); return status;
  };
  assert.equal(await post({ page: 'one', session: 's1', focused: true }, 'http://evil.test'), 403);
  assert.equal(await post({ page: 'one', session: 's1', focused: true }), 204);
  assert.equal(views.get('one').session, 's1');
  await post({ page: 'one', session: 's2', focused: true }); assert.equal(views.get('one').session, 's2');
  await post({ page: 'one', session: '', focused: false }); assert.equal(views.size, 0);
  await post({ page: 'one', session: 's1', focused: true }); time += 10001;
  await post({ page: 'two', session: 's2', focused: true }); assert.equal(views.has('one'), false);
  for (let i = 0; i < 70; i++) await post({ page: `page-${i}`, session: 's1', focused: true });
  assert.equal(views.size, 64); assert.ok(changed > 0);
});

test('V3 and V4 session notifications use live events without synchronous history access', () => {
  for (const version of [3, 4]) {
    const events = [];
    const tracker = createTracker(e => events.push(e));
    const current = { header: { id: `v${version}`, version, origin: 'user' } };
    for (const key of ['snapshotEvents', 'eventAt', 'ownEvents', 'events']) {
      Object.defineProperty(current, key, { get() { throw Error(`obsolete history access: ${key}`); } });
    }
    tracker.event(current, { type: 'turn/start', data: { turn: 1 } });
    tracker.event(current, { type: 'turn/end', data: { turn: 1, reason: { kind: 'completed' } } });
    assert.equal(events.length, 1);
    assert.equal(events[0].session, `v${version}`);
    assert.equal(events[0].kind, 'completed');
    tracker.dispose();
  }
});