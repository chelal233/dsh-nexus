import test from 'node:test';
import assert from 'node:assert/strict';
import { harnessUrl, githubUrl, trustedFrame, validateRequest } from '../electron/policy.mjs';

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

test('sandbox preload forwards every permitted desktop command and rejects unknown commands', async () => {
  const { readFileSync } = await import('node:fs');
  const { runInNewContext } = await import('node:vm');
  const { commands } = await import('../electron/policy.mjs');
  let bridge;const forwarded=[];
  runInNewContext(readFileSync(new URL('../electron/preload.cjs',import.meta.url),'utf8'),{
    process:{argv:[]},require:name=>{
      assert.equal(name,'electron');
      return {contextBridge:{exposeInMainWorld:(_name,value)=>{bridge=value;}},ipcRenderer:{invoke:async(channel,command,args)=>{forwarded.push({channel,command,args});return {value:'ok'};}}};
    },
  });
  for(const command of commands) assert.equal(await bridge.invoke(command,{version:'0.2.0'}),'ok',command);
  assert.equal(forwarded.length,commands.size);
  assert.ok(forwarded.every(call=>call.channel==='nexus:command'));
  await assert.rejects(bridge.invoke('exec',{}),/Unknown desktop command/);
});

test('GitHub external links accept HTTPS GitHub only', () => {
  assert.equal(githubUrl('https://github.com/example/plugin').href, 'https://github.com/example/plugin');
  for (const url of ['file:///tmp/x', 'javascript:alert(1)', 'http://github.com/a/b', 'https://github.com.evil/a/b', 'https://a:b@github.com/a/b', 'https://github.com:444/a/b']) assert.throws(() => githubUrl(url));
  validateRequest('open_github', { url: 'https://github.com/example/plugin' });
});
