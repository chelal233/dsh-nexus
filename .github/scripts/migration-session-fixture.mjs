import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';

export const DEFAULT_WORKSPACE = '/qa/test/fixture-workspace';
export const SESSION_TEXT = 'Synthetic valid offline migration session. No model request.';

// Frozen rc.2 format.ts, 639ed015, projectKey: UTF-16 units, not code points.
export function frozenProjectKey(cwd) {
  assert.ok(typeof cwd === 'string' && cwd.length > 0);
  let readable = '', separatorRun = false;
  for (let i = 0; i < cwd.length; i++) {
    const code = cwd.charCodeAt(i), ch = String.fromCharCode(code);
    if (ch === '/' || ch === '\\' || ch === ':') {
      if (!separatorRun) readable += '-';
      separatorRun = true;
    } else if (ch !== '~' && /^[A-Za-z0-9._-]$/.test(ch)) {
      readable += ch; separatorRun = false;
    } else {
      readable += '~' + code.toString(16).toUpperCase().padStart(4, '0');
      separatorRun = false;
    }
  }
  const slug = readable.replace(/^-+/, '') || 'root';
  return `--${slug.slice(0, 251)}--`;
}

export function migrationSessionFixture(cwd = DEFAULT_WORKSPACE) {
  const header = { type: 'session', version: 4, id: 'qa-valid-session', createdAt: 1,
    cwd, isSeeded: false, delegationDepth: 0 };
  const events = [
    { type: 'user/message', seq: 0, time: 2, data: { content: [{ type: 'text', text: SESSION_TEXT }],
      source: { kind: 'user' }, role: 'user', id: 'qa-valid-message' }, surfaceOp: 'append' },
    { type: 'session/end-seed', seq: 1, time: 3, data: {} },
    { type: 'permission/preset', seq: 2, time: 4, data: { preset: 'workspace-write' } },
    { type: 'sandbox/mode', seq: 3, time: 5, data: { mode: 'workspace-write' } },
    { type: 'approval/policy', seq: 4, time: 6, data: { policy: 'ask' } },
    { type: 'session/end-seed', seq: 5, time: 7, data: {} },
  ];
  const plaintext = [header, ...events].map(value => JSON.stringify(value) + '\n').join('');
  return { cwd, directory: `sessions/${frozenProjectKey(cwd)}/qa-valid-session`, plaintext,
    bytes: Buffer.byteLength(plaintext), sha256: createHash('sha256').update(plaintext).digest('hex') };
}
