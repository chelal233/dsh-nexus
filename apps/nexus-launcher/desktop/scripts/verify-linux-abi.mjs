import { execFileSync } from 'node:child_process';
import path from 'node:path';
import assert from 'node:assert/strict';

const root = path.resolve(process.argv[2]);
for (const name of ['nexus-agent', 'nexus-launcher', 'nexusctl', 'nexus-desktop-bridge']) {
  const file = path.join(root, name);
  assert.match(execFileSync('readelf', ['-h', file], { encoding: 'utf8' }), /Machine:\s+AArch64/);
  const versions = execFileSync('readelf', ['--version-info', file], { encoding: 'utf8' });
  for (const match of versions.matchAll(/Name: GLIBC_(\d+)\.(\d+)/g)) {
    assert.ok(Number(match[1]) < 2 || Number(match[1]) === 2 && Number(match[2]) <= 28, `${name} requires ${match[0]}`);
  }
}
console.log('Native ARM64 Rust executables meet the glibc 2.28 ceiling.');
