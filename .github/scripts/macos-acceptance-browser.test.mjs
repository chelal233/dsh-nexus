import assert from 'node:assert/strict';
import { test } from 'node:test';
import { browserLease } from './macos-acceptance-browser.mjs';

function nativeFixture(initial = ['com.apple.Safari', 'com.apple.Safari']) {
  const handlers = new Map(['http', 'https'].map((scheme, i) => [scheme, initial[i]])), writes = [];
  let reject;
  const command = args => {
    if (args[0] === 'read') return JSON.stringify([...handlers]);
    assert.equal(args[0], 'set'); const [, scheme, handler] = args; writes.push([scheme, handler]);
    if (reject?.(scheme, handler)) return -50;
    handlers.set(scheme, handler.toLowerCase()); return 0;
  };
  return { handlers, writes, command, reject: callback => { reject = callback; } };
}

test('browser lease restores prior handlers despite native ID case folding', () => {
  const native = nativeFixture(), lease = browserLease(native.command);
  lease.setChrome(); assert.equal(native.handlers.get('http'), 'com.google.chrome');
  lease.restore(); assert.equal(native.handlers.get('https'), 'com.apple.safari');
});

test('an unset second scheme stops before changing the valid first scheme', () => {
  const native = nativeFixture(['com.apple.Safari', null]);
  assert.throws(() => browserLease(native.command), /unset/); assert.deepEqual(native.writes, []);
});

test('a partially rejected change can restore both saved handlers', () => {
  const native = nativeFixture(), lease = browserLease(native.command);
  native.reject((scheme, handler) => scheme === 'https' && handler === 'com.google.Chrome');
  assert.throws(() => lease.setChrome(), /rejected https/); lease.restore();
  assert.deepEqual([...native.handlers.values()], ['com.apple.safari', 'com.apple.safari']);
});

test('restoration attempts both schemes and reports refusal plus mismatched readback', () => {
  const native = nativeFixture(), lease = browserLease(native.command); lease.setChrome();
  native.reject((scheme, handler) => scheme === 'http' && handler === 'com.apple.Safari');
  assert.throws(() => lease.restore(), /rejected http.*readback differs/s);
  assert.equal(native.handlers.get('https'), 'com.apple.safari');
});

test('existing Chrome needs no preference write and a different existing browser is blocked without mutation', () => {
  const chrome = nativeFixture(['com.google.Chrome', 'com.google.chrome']);
  browserLease(chrome.command).requireExistingChrome(); assert.deepEqual(chrome.writes, []);
  const safari = nativeFixture();
  assert.throws(() => browserLease(safari.command).requireExistingChrome(), { code: 'BROWSER_CONFIGURATION_BLOCKED' });
  assert.deepEqual(safari.writes, []);
});
