import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { declarationChecks, boundDeclarationReport } from '../src/compatibility.mjs';

test('large declaration reports bound UTF-8 bytes including the cache envelope and retain mismatch rows first', () => {
  const rows = Array.from({ length: 60 }, (_, i) => ({ package: `plugin-${i}`, version: '1.0.0', status: i === 59 ? 'mismatch' : 'match',
    declarations: Array.from({ length: 8 }, () => ({ dependency: '@deepseek-ai/dsh-tools', required: '>=0.1.0', actual: '0.1.6-alpha.1', status: 'match' })) }));
  rows.push({ package: 'huge', status: 'unknown', declarations: [{ required: '界'.repeat(70000) }] });
  const result = boundDeclarationReport({ status: 'passed', declarations: rows });
  assert.equal(result.status, 'passed');
  assert.equal(result.declarations[0].package, 'plugin-59');
  assert.ok(result.declarations_omitted > 0);
  assert.equal(result.declarations.length + result.declarations_omitted, rows.length);
  assert.ok(Buffer.byteLength(JSON.stringify({ key: '0'.repeat(64), report: result }, null, 2)) <= 32768);
  const failed = boundDeclarationReport({ ...result, error: '界'.repeat(4600), status: 'needs_choice' });
  assert.ok(Buffer.byteLength(JSON.stringify(failed, null, 2)) < 65536);
  assert.equal(failed.declarations.length + failed.declarations_omitted, rows.length);
});

test('local declarations distinguish prereleases, target peers, unknown and disabled plugins without mutations', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nexus-declarations-'));
  const slot = path.join(root, 'release'), dir = path.join(root, 'profile');
  const write = (file, value) => { fs.mkdirSync(path.dirname(file), { recursive: true }); fs.writeFileSync(file, JSON.stringify(value)); };
  const host = path.join(slot, 'apps/cli/package.json');
  write(host, { version: '0.1.6-alpha.1' });
  const manifests = {
    matched: { version: '1.0.0', engines: { dsh: '>=0.1.6-alpha.0 <0.2.0' } },
    stableOnly: { engines: { dsh: '>=0.1.5 <0.2.0' } },
    peer: { peerDependencies: { '@deepseek-ai/dsh-tools': '^0.2.0' } },
    optional: { peerDependencies: { '@deepseek-ai/dsh-tools': '^0.2.0' }, peerDependenciesMeta: { '@deepseek-ai/dsh-tools': { optional: true } } },
    invalid: { engines: { dsh: 'not a range' } },
    nested: { dsh: { engines: { dsh: '0.1.6-alpha.1' } } },
    undeclared: { version: '2.0.0' },
    disabled: { engines: { dsh: '>99.0.0' } },
  };
  for (const [name, value] of Object.entries(manifests)) write(path.join(dir, 'node_modules', name, 'package.json'), value);
  const tools = path.join(slot, 'tools'); write(path.join(tools, 'package.json'), { version: '0.1.6-alpha.1' });
  const source = { dir, manualDisabled: ['disabled'], manifest: { dsh: { profile: { bundles: [...Object.keys(manifests), 'missing'] } } } };
  const originalFetch = globalThis.fetch;
  globalThis.fetch = () => { throw Error('Declaration checks must not access the network'); };
  try {
    const result = declarationChecks(source, slot, new Map([['@deepseek-ai/dsh-tools', tools]]));
    assert.deepEqual(Object.fromEntries(result.map(r => [r.package, r.status])), {
      matched: 'match', stableOnly: 'mismatch', peer: 'mismatch', optional: 'unknown', invalid: 'unknown', nested: 'match', undeclared: 'unknown', missing: 'unknown',
    });
    assert.equal(result.find(r => r.package === 'optional').declarations[0].optional, true);
    for (const [name, value] of Object.entries(manifests)) assert.deepEqual(JSON.parse(fs.readFileSync(path.join(dir, 'node_modules', name, 'package.json'))), value);
    write(host, { version: '0.1.5' });
    assert.equal(declarationChecks(source, slot, new Map())[0].status, 'mismatch');
    fs.writeFileSync(host, '{broken');
    assert.equal(declarationChecks(source, slot, new Map())[0].status, 'unknown');
  } finally { globalThis.fetch = originalFetch; fs.rmSync(root, { recursive: true, force: true }); }
});
