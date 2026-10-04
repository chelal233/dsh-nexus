import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { setTimeout as delay } from 'node:timers/promises';
import { test } from 'node:test';
import { completedDevToolsPort, releaseOwnedChildHandles } from './linux-rpm-preflight-lifecycle.mjs';

test('empty or partially written DevTools first line remains pending', () => {
  for (const text of ['', '4', '41299']) assert.equal(completedDevToolsPort(text), null);
  assert.equal(completedDevToolsPort('41299\n/devtools/browser/fixture'), '41299');
});

test('completed malformed DevTools first line still fails', () => {
  for (const text of ['\n', 'invalid\n', '0\n', '65536\n']) {
    assert.throws(() => completedDevToolsPort(text));
  }
});

test('failed cleanup releases owned handles without claiming child termination', async () => {
  const helper = new URL('./linux-rpm-preflight-lifecycle.mjs', import.meta.url).href;
  const owner = `import { spawn } from 'node:child_process';
    import { releaseOwnedChildHandles } from ${JSON.stringify(helper)};
    const child = spawn(process.execPath, ['-e', 'setTimeout(() => process.exit(0), 2500)'], { stdio: ['ignore', 'pipe', 'pipe'] });
    child.once('spawn', () => {
      const report = { result: 'FAIL', cleanupError: 'Owned child did not confirm close', exitCode: child.exitCode, signalCode: child.signalCode };
      releaseOwnedChildHandles(child);
      console.log(JSON.stringify(report));
      process.exitCode = 1;
    });`;
  const child = spawn(process.execPath, ['--input-type=module', '-e', owner], { stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '';
  child.stdout.on('data', data => { output += data; });
  const close = once(child, 'close');
  try {
    await Promise.race([close, delay(1500).then(() => { throw new Error('Owned handles still block failure return'); })]);
    assert.equal(child.exitCode, 1);
    assert.deepEqual(JSON.parse(output), { result: 'FAIL', cleanupError: 'Owned child did not confirm close', exitCode: null, signalCode: null });
  } finally {
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
    releaseOwnedChildHandles(child);
    // The only fixture descendant exits itself within a fixed 2.5 seconds.
    await delay(3000);
  }
});
