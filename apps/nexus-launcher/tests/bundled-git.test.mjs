import test from 'node:test';
import assert from 'node:assert/strict';
import { gitDistribution, verifyLinuxGitAbi, stageGitNotices, excludeGitCredentialManager, gitCredentialManagerExcluded } from '../desktop/scripts/bundled-git.mjs';
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

import { createHash } from 'node:crypto';
test('Git notice staging rejects altered materials before copying', t => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nexus-git-notices-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const input = path.join(root, 'materials'), output = path.join(root, 'output');
  fs.mkdirSync(input);
  fs.writeFileSync(path.join(input, 'LICENSE.txt'), 'original notice');
  fs.writeFileSync(path.join(input, 'manifest.json'), JSON.stringify({ files: [{ path: 'LICENSE.txt', sha256: createHash('sha256').update('original notice').digest('hex') }] }));
  assert.equal(stageGitNotices(input, output), 1);
  assert.equal(fs.readFileSync(path.join(output, 'LICENSE.txt'), 'utf8'), 'original notice');
  fs.writeFileSync(path.join(input, 'LICENSE.txt'), 'altered');
  assert.throws(() => stageGitNotices(input, path.join(root, 'rejected')), /checksum/);
  assert.equal(fs.existsSync(path.join(root, 'rejected')), false);
});

test('GCM exclusion checks every file before removal and preserves Git and other helpers', t => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nexus-gcm-exclude-'));
  t.after(() => fs.rmSync(root, {recursive:true,force:true}));
  fs.mkdirSync(path.join(root,'mingw64/bin'),{recursive:true});fs.mkdirSync(path.join(root,'etc'));
  fs.writeFileSync(path.join(root,'etc/gitconfig'),'[credential]\n helper = manager\n helper = other\n');
  const names=['git-credential-manager.exe','dependency.dll'];
  for(const name of [...names,'git.exe'])fs.writeFileSync(path.join(root,'mingw64/bin',name),name);
  const record={target:'windows-x64',files:names.map(name=>({name,present:true,matches:true,sha256:createHash('sha256').update(name).digest('hex')}))};
  const changed=structuredClone(record);changed.files[1].sha256='0'.repeat(64);
  assert.throws(()=>excludeGitCredentialManager(root,[changed],{platform:'win32',arch:'x64'}),/identity mismatch/);
  assert.ok(fs.existsSync(path.join(root,'mingw64/bin',names[0])));
  assert.equal(excludeGitCredentialManager(root,[record],{platform:'win32',arch:'x64'}),2);
  assert.ok(gitCredentialManagerExcluded(root,[record],{platform:'win32',arch:'x64'}));
  assert.ok(fs.existsSync(path.join(root,'mingw64/bin/git.exe')));
  assert.equal(fs.readFileSync(path.join(root,'etc/gitconfig'),'utf8'),'[credential]\n helper = other\n');
});
