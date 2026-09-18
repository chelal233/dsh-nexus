import test from 'node:test';
import assert from 'node:assert/strict';
import { inspectFile } from './check-source-privacy.mjs';

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
