import test from 'node:test';
import assert from 'node:assert/strict';
import { settings, shouldNotify, EventCursor, notificationContent, taskFocused } from '../electron/notifications.mjs';

test('native notices include conversation identity and context with status fallbacks', () => {
  const message = notificationContent('completed', 'zh', { title: '修复通知', session: 's1', body: '全部测试通过。' });
  assert.equal(message.title, '修复通知 · 回合已完成'); assert.equal(message.body, '全部测试通过。');
  assert.match(notificationContent('failed', 'zh', { session: 's1', detailCode: 'max-tokens' }).body, /输出长度/);
  assert.equal(notificationContent('question', 'en').body, 'Answer needed');
  assert.doesNotMatch(notificationContent('failed', 'zh', { body: '\x1b\x07bad' }).body, /[\x00-\x1f]/);
});

test('only fresh focus on the matching Harness conversation suppresses task notifications', () => {
  const task = { session: 's1' }, now = 10000;
  assert.equal(taskFocused(task, [], now), false); // Launcher foreground is irrelevant.
  assert.equal(taskFocused(task, [{ session: 's2', focused: true, time: now }], now), false);
  assert.equal(taskFocused(task, [{ session: 's1', focused: true, time: now }], now), true);
  assert.equal(taskFocused(task, [{ session: 's1', focused: false, time: now }], now), false);
  assert.equal(taskFocused(task, [{ session: 's1', focused: true, time: 0 }], now), false);
});
test('notification modes, per-category switches and channel preferences are independent', () => {
  const config = settings({ desktop: 'unfocused', terminal: 'always', categories: { question: false } });
  assert.equal(shouldNotify(config, 'completed', true), false);
  assert.equal(shouldNotify(config, 'completed', false), true);
  assert.equal(shouldNotify(config, 'completed', undefined), false);
  assert.equal(shouldNotify(config, 'question', false), false);
  assert.equal(shouldNotify(config, 'completed', true, 'terminal'), true);
  assert.equal(shouldNotify(settings({ desktop: 'off' }), 'failed', false), false);
});
test('startup and new producer generations skip history; repeated polls emit each new event once', () => {
  const cursor = new EventCursor();
  assert.deepEqual(cursor.consume({ epoch: 'a', sequence: 8, events: [{sequence:8,kind:'completed'}] }), []);
  const state = { epoch: 'a', sequence: 9, events: [{sequence:9,kind:'completed'}] };
  assert.equal(cursor.consume(state).length, 1);
  assert.deepEqual(cursor.consume(state), []);
  assert.deepEqual(cursor.consume({ epoch: 'b', sequence: 1, events: [{sequence:1,kind:'completed'}] }), []);
});
test('events produced after launcher startup survive the first poll', () => {
  const cursor = new EventCursor();
  const snapshot = { epoch: 'fresh', sequence: 2, events: [
    { sequence: 1, kind: 'completed', time: cursor.startedAt - 1 },
    { sequence: 2, kind: 'question', time: cursor.startedAt + 1 },
  ] };
  assert.deepEqual(cursor.consume(snapshot).map(e => e.kind), ['question']);
  assert.deepEqual(cursor.consume(snapshot), []);
});

test('all nine category switches and channel modes obey the notification policy', () => {
 const kinds=['completed','failed','approval','question','blocked','job-completed','job-failed','harness-failed','update-ready'];
 for(const kind of kinds)for(const channel of ['desktop','terminal'])for(const enabled of [false,true]) {
  for(const [mode,focus,expected] of [['off',false,false],['off',true,false],['always',false,true],['always',true,true],['always',undefined,true],['unfocused',false,true],['unfocused',true,false],['unfocused',undefined,false]]) {
   const config=settings({[channel]:mode,categories:{[kind]:enabled}});
   assert.equal(shouldNotify(config,kind,focus,channel),enabled&&expected,`${kind}/${channel}/${mode}/${focus}/${enabled}`);
  }
 }
});
