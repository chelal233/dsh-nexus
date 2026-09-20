import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { inspectFile, checkRepository } from './check-source-privacy.mjs';

test('reject private paths without returning their contents', () => {
  const privatePath = 'C:' + String.raw`\Users\Alice\project`;
  const hits = inspectFile('docs/history/report.md', privatePath);
  assert.deepEqual(hits.map(x => x.rule), ['absolute-document-path', 'personal-home-path']);
  assert.ok(!JSON.stringify(hits).includes('Alice'));
  assert.equal(inspectFile('source.rs', '/home/' + 'alice/project')[0].rule, 'personal-home-path');
});

test('allow public URLs, placeholders and deliberate test users', () => {
  assert.deepEqual(inspectFile('README.md', 'https://example.com\n<USER_HOME>/bin'), []);
  assert.deepEqual(inspectFile('test.rs', 'C:' + String.raw`\Users\Fixture\bin`), []);
  assert.deepEqual(inspectFile('.env.example', 'KEY=<YOUR_KEY>'), []);
  assert.deepEqual(inspectFile('test.mjs', "path.resolve('fixture/home/profiles/web')"), []);
  assert.deepEqual(inspectFile('test.mjs', 'C:' + '/home/' + 'keys/private.pem'), []);
});

test('reject internal records and private file names even for binary contents', () => {
  for (const name of ['.agent-memory/status.md', '.env', '.env.local', 'keys/signing.pfx']) {
    assert.ok(inspectFile(name, '\0').length > 0);
  }
});

test('local pre-publish check includes new files and tolerates pending tracked deletions',t=>{
 const root=fs.mkdtempSync(path.join(os.tmpdir(),'nexus-privacy-'));t.after(()=>fs.rmSync(root,{recursive:true,force:true}));
 const git=(...args)=>execFileSync('git',args,{cwd:root,stdio:'pipe',windowsHide:true});git('init');
 fs.writeFileSync(path.join(root,'old.txt'),'safe');git('add','old.txt');fs.unlinkSync(path.join(root,'old.txt'));
 assert.doesNotThrow(()=>checkRepository(root));
 fs.writeFileSync(path.join(root,'new.md'),'C:'+String.raw`\Users\Alice\local`);
 const prior=console.error;console.error=()=>{};
 try {assert.throws(()=>checkRepository(root),/Source privacy check failed/);}finally{console.error=prior;}
});
