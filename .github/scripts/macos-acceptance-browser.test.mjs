import assert from 'node:assert/strict';
import vm from 'node:vm';
import { test } from 'node:test';
import { browserLease } from './macos-acceptance-browser.mjs';

function nativeFixture(initial = ['com.apple.Safari', 'com.apple.Safari']) {
  const handlers = new Map(['http', 'https'].map((scheme, i) => [scheme, initial[i]])), writes = [];
  let reject;
  const wrap = value => ({ value, isNil: () => value == null });
  const cfString = value => { assert.equal(typeof value, 'object', 'Must wrap CFStringRef argument'); return value.value; };
  wrap.LSCopyDefaultHandlerForURLScheme = scheme => wrap(handlers.get(cfString(scheme)));
  wrap.LSSetDefaultHandlerForURLScheme = (schemeValue, handlerValue) => {
    const scheme = cfString(schemeValue), handler = cfString(handlerValue); writes.push([scheme, handler]);
    if (reject?.(scheme, handler)) return -50;
    handlers.set(scheme, handler.toLowerCase()); return 0;
  };
  return { handlers, writes, reject: callback => { reject = callback; },
    jxa: script => String(vm.runInNewContext(script, { $: wrap, ObjC: { import() {}, unwrap: cfString } })) };
}

test('CFString parameters roundtrip and restore exact prior handlers despite native ID case folding', () => {
  const native = nativeFixture(), lease = browserLease(native.jxa);
  lease.setChrome(); assert.equal(native.handlers.get('http'), 'com.google.chrome');
  lease.restore(); assert.equal(native.handlers.get('https'), 'com.apple.safari');
});

test('an unset second scheme stops before changing the valid first scheme', () => {
  const native = nativeFixture(['com.apple.Safari', null]);
  assert.throws(() => browserLease(native.jxa), /unset/); assert.deepEqual(native.writes, []);
});

test('a partially rejected change can restore both saved handlers', () => {
  const native = nativeFixture(), lease = browserLease(native.jxa);
  native.reject((scheme, handler) => scheme === 'https' && handler === 'com.google.Chrome');
  assert.throws(() => lease.setChrome(), /rejected https/); lease.restore();
  assert.deepEqual([...native.handlers.values()], ['com.apple.safari', 'com.apple.safari']);
});

test('restoration attempts both schemes and reports refusal plus mismatched readback', () => {
  const native = nativeFixture(), lease = browserLease(native.jxa); lease.setChrome();
  native.reject((scheme, handler) => scheme === 'http' && handler === 'com.apple.Safari');
  assert.throws(() => lease.restore(), /rejected http.*readback differs/s);
  assert.equal(native.handlers.get('https'), 'com.apple.safari');
});
