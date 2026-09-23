import test from 'node:test';
import assert from 'node:assert/strict';
import { gitDistribution, verifyLinuxGitAbi } from '../desktop/scripts/bundled-git.mjs';
import { targets } from '../desktop/scripts/release-platform.mjs';

test('every supported release has a pinned native Git distribution', () => {
  for (const spec of Object.values(targets)) {
    const dist = gitDistribution(spec);
    assert.match(dist.sha256, /^[a-f0-9]{64}$/);
    assert.ok(dist.url.startsWith('https://github.com/desktop/dugite-native/releases/download/'));
    assert.ok(dist.archive.includes(spec.arch));
    assert.equal(dist.entry, spec.platform === 'win32' ? 'git/cmd/git.exe' : 'git/bin/git');
  }
  assert.throws(() => gitDistribution({ platform: 'linux', arch: 'mips64' }), /Unsupported/);
});

import fs from 'node:fs';import os from 'node:os';import path from 'node:path';
test('Linux Git checks helper architecture and versioned libc requirements',t=>{
 const root=fs.mkdtempSync(path.join(os.tmpdir(),'nexus-git-abi-'));t.after(()=>fs.rmSync(root,{recursive:true,force:true}));
 const header=Buffer.alloc(64);Buffer.from([127,69,76,70]).copy(header);header[5]=1;header.writeUInt16LE(183,18);
 fs.writeFileSync(path.join(root,'git'),Buffer.concat([header,Buffer.from('GLIBC_2.34\0')]));assert.equal(verifyLinuxGitAbi(root),1);
 fs.writeFileSync(path.join(root,'helper'),Buffer.concat([header,Buffer.from('GLIBC_2.35\0')]));assert.throws(()=>verifyLinuxGitAbi(root),/exceeds glibc/);
 header.writeUInt16LE(62,18);fs.writeFileSync(path.join(root,'helper'),header);assert.throws(()=>verifyLinuxGitAbi(root),/not ARM64/);
});
