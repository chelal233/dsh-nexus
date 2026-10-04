import test from 'node:test';
import assert from 'node:assert/strict';
import { verifyMacHostSignature } from '../scripts/offline-package.mjs';

test('portable macOS export keeps strict verification and the existing timeout', () => {
  verifyMacHostSignature('/fixture/Nexus Launcher.app', (program, args, options) => {
    assert.equal(program, '/usr/bin/codesign');
    assert.deepEqual(args, ['--verify', '--deep', '--strict', '/fixture/Nexus Launcher.app']);
    assert.deepEqual(options, { encoding: 'utf8', timeout: 30000 });
    return { status: 0, signal: null, stderr: '' };
  });
});

test('portable macOS export distinguishes a timeout without accepting it', () => {
  const error = Object.assign(new Error('spawn timed out'), { code: 'ETIMEDOUT' });
  assert.throws(() => verifyMacHostSignature('/fixture', () => ({ status: null, signal: 'SIGTERM', error })), result =>
    /verification timed out/.test(result.message) && /ETIMEDOUT/.test(result.message) && result.cause === error);
});

test('portable macOS export retains bounded failure details and rejects signal termination', () => {
  assert.throws(() => verifyMacHostSignature('/fixture', () => ({ status: 1, signal: null, stderr: 'a sealed resource is missing or invalid\u0000' })), /status 1, signal none, error none.*sealed resource is missing or invalid$/);
  assert.throws(() => verifyMacHostSignature('/fixture', () => ({ status: null, signal: 'SIGKILL', stderr: 'x'.repeat(10000) })), result =>
    /signal SIGKILL/.test(result.message) && result.message.length < 4300);
});
