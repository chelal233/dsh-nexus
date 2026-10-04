import assert from 'node:assert/strict';

const schemes = ['http', 'https'];

// The native helper reads/copies CFStringRef and serializes it with Foundation.
export function browserLease(command) {
  const read = () => JSON.parse(command(['read']));
  const before = read();
  assert.deepEqual(before.map(item => item[0]), schemes);
  // Validate both original values before changing either; there is no public clear-handler operation.
  for (const [, handler] of before) {
    assert.equal(typeof handler, 'string', 'Default browser is unset; cannot prove exact restoration');
    assert.match(handler, /^[A-Za-z0-9.-]+$/);
  }
  const write = (scheme, handler) => assert.equal(Number(command(['set', scheme, handler])),
  0, `LaunchServices rejected ${scheme} handler`);
  const verify = expected => assert.deepEqual(read().map(([scheme, handler]) => [scheme, handler?.toLowerCase()]),
    expected.map(([scheme, handler]) => [scheme, handler.toLowerCase()]), 'Default browser readback differs');
  return {
    before,
    setChrome() { for (const scheme of schemes) write(scheme, 'com.google.Chrome'); verify(schemes.map(s => [s, 'com.google.Chrome'])); },
    restore() {
      const failures = [];
      for (const [scheme, handler] of before) try { write(scheme, handler); } catch (error) { failures.push(error); }
      try { verify(before); } catch (error) { failures.push(error); }
      if (failures.length) throw new AggregateError(failures, failures.map(error => error.message).join('; '));
    },
  };
}
