import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { nativeEditAction } from '../electron/policy.mjs';

test('menu-less Mac windows retain editing without intercepting other platforms or shortcuts', () => {
  const input = { type: 'keyDown', meta: true, key: 'z' };
  assert.equal(nativeEditAction('darwin', input), 'undo');
  assert.equal(nativeEditAction('darwin', { ...input, shift: true }), 'redo');
  assert.equal(nativeEditAction('darwin', { ...input, key: 'C' }), 'copy');
  assert.equal(nativeEditAction('win32', input), undefined);
  assert.equal(nativeEditAction('darwin', { ...input, alt: true }), undefined);
  assert.equal(nativeEditAction('darwin', { ...input, type: 'keyUp' }), undefined);
});

test('notification monitor uses BEL in Apple Terminal and unknown terminals, OSC only for known support', () => {
  const code = readFileSync(new URL('../../../plugins/nexus-notifications/terminal/monitor.mjs', import.meta.url), 'utf8');
  for (const [env, expected] of [[{ TERM_PROGRAM: 'Apple_Terminal' }, '\x07'], [{ TERM_PROGRAM: 'unknown' }, '\x07'], [{ TERM_PROGRAM: 'iTerm.app' }, '\x1b]9;'], [{ WT_SESSION: 'test' }, '\x1b]9;']]) {
    let poll, sequence = 0, output = '';
    const fs = { statSync: () => ({ size: 100 }), readFileSync: file => JSON.stringify(file === 'prefs'
      ? { terminal: 'always', method: 'auto' }
      : { epoch: 'run', sequence, events: [{ sequence, kind: 'completed' }] }) };
    vm.runInNewContext(code.replace("import fs from 'node:fs';", ''), {
      fs, console: { log() {}, error() {} }, setInterval: callback => { poll = callback; },
      process: { argv: ['node', 'monitor', 'events', 'prefs'], env, on() {},
        stdin: { isTTY: true, setRawMode() {}, resume() {}, on() {} },
        stdout: { isTTY: true, write: text => { output += text; } } },
    });
    poll(); sequence++; output = ''; poll();
    assert.ok(output.startsWith(expected), JSON.stringify(env));
  }
});
