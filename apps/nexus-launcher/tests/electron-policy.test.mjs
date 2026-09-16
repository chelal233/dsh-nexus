import test from 'node:test';
import assert from 'node:assert/strict';
import { harnessUrl, trustedFrame, validateRequest } from '../electron/policy.mjs';

test('Harness navigation accepts loopback metadata only', () => {
  for (const url of ['http://127.0.0.1:1234/?token=abc', 'http://[::1]:1234/']) assert.ok(harnessUrl(url));
  for (const url of ['file:///etc/passwd', 'https://example.com', 'http://127.0.0.1.evil:1234/', 'http://a:b@localhost:1234/', 'javascript:alert(1)']) {
    assert.throws(() => harnessUrl(url));
  }
});
test('Subframes and a navigated or foreign renderer cannot invoke desktop capabilities', () => {
  const frame = { url: 'file:///launcher/index.html' };
  const contents = { mainFrame: frame };
  const window = { isDestroyed: () => false, webContents: contents };
  const event = { sender: contents, senderFrame: frame };
  assert.equal(trustedFrame(event, window, frame.url), true);
  assert.equal(trustedFrame({ ...event, senderFrame: { ...frame } }, window, frame.url), false);
  assert.equal(trustedFrame({ ...event, sender: {} }, window, frame.url), false);
  assert.equal(trustedFrame(event, window, 'http://localhost/'), false);
});
test('Desktop boundary rejects arbitrary channels, malformed and oversized requests', () => {
  validateRequest('proxy_request', { path: '/v1/state', method: 'GET' });
  assert.throws(() => validateRequest('exec', {}));
  assert.throws(() => validateRequest('proxy_request', []));
  assert.throws(() => validateRequest('notify', { title: 'x'.repeat(32769) }));
});
