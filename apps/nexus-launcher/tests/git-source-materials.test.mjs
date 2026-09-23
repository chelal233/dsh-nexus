import test from 'node:test';
import assert from 'node:assert/strict';
import { validateSources } from '../desktop/scripts/prepare-git-sources.mjs';
test('source companion refuses uncleared, escaping, duplicate or unpinned materials', () => {
  const entry = { file: 'grep-1~3.0.src.tar.gz', url: 'https://example.org/git.tar.gz', size: 1, sha256: 'a'.repeat(64) };
  assert.doesNotThrow(() => validateSources({ schema: 1, complete: true, files: [entry] }));
  for (const value of [{schema:1,complete:false,files:[entry]},{schema:1,complete:true,files:[]},{schema:1,complete:true,files:[entry,entry]},
    ...[{file:'../git.tar.gz'},{sha256:'wrong'},{url:'http://example.org/git.tar.gz'},{size:0}].map(change=>({schema:1,complete:true,files:[{...entry,...change}]}))]) {
    assert.throws(() => validateSources(value));
  }
});
