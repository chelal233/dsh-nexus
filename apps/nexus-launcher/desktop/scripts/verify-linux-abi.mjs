import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import assert from 'node:assert/strict';

export function verifyLinuxAbi(root, arch = process.arch, readelf = args => execFileSync('readelf', args, { encoding: 'utf8' })) {
  const machine = { x64: /Machine:\s+Advanced Micro Devices X86-64/, arm64: /Machine:\s+AArch64/ }[arch];
  if (!machine) throw new Error(`Unsupported Linux executable architecture: ${arch}`);
  for (const name of ['nexus-agent', 'nexus-launcher', 'nexusctl', 'nexus-desktop-bridge']) {
    const file = path.join(root, name);
    assert.match(readelf(['-h', file]), machine);
    const dynamic = readelf(['-d', file]);
    assert.doesNotMatch(dynamic, /Shared library: \[lib(?:ssl|crypto)\./, `${name} must bundle OpenSSL statically`);
    const versions = readelf(['--version-info', file]);
    for (const match of versions.matchAll(/Name: GLIBC_(\d+)\.(\d+)/g)) {
      assert.ok(Number(match[1]) < 2 || Number(match[1]) === 2 && Number(match[2]) <= 28, `${name} requires ${match[0]}`);
    }
  }
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  verifyLinuxAbi(path.resolve(process.argv[2]));
  console.log(`Native ${process.arch} Rust executables meet the glibc 2.28 ceiling.`);
}
