import test from 'node:test';
import assert from 'node:assert/strict';
import {stopDesktopChild} from '../electron/desktop-process.mjs';

const denied = () => Object.assign(new Error('kill EPERM'), {code: 'EPERM'});
function fixture(t, kill) {
  const descriptor = Object.getOwnPropertyDescriptor(process, 'platform');
  Object.defineProperty(process, 'platform', {...descriptor, value: 'darwin'});
  t.after(() => Object.defineProperty(process, 'platform', descriptor));
  t.mock.method(process, 'kill', kill);
}
test('Darwin waits for a denied post-TERM probe to become absent without another signal', async t => {
  const calls = [];
  fixture(t, (pid, signal) => {
    assert.equal(pid, -12345); calls.push(signal);
    if (calls.length === 2) throw denied();
    if (calls.length === 3) throw Object.assign(new Error('absent'), {code: 'ESRCH'});
    return true;
  });
  await stopDesktopChild({pid: 12345});
  assert.deepEqual(calls, ['SIGTERM', 0, 0]);
});
test('Darwin initial termination permission denial remains fatal', async t => {
  const error = denied(), calls = [];
  fixture(t, (pid, signal) => { calls.push(signal); throw error; });
  await assert.rejects(stopDesktopChild({pid: 12345}), e => e === error);
  assert.deepEqual(calls, ['SIGTERM']);
});
test('Darwin persistent probe denial remains fatal at the bounded deadline', async t => {
  const error = denied(), calls = []; let time = 0;
  t.mock.method(Date, 'now', () => { time += 10001; return time; });
  fixture(t, (pid, signal) => { calls.push(signal); if (signal === 0) throw error; return true; });
  await assert.rejects(stopDesktopChild({pid: 12345}), e => e === error);
  assert.deepEqual(calls, ['SIGTERM', 0]);
});
