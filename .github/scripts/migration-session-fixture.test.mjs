import assert from 'node:assert/strict';
import { test } from 'node:test';
import { migrationSessionFixture, frozenProjectKey } from './migration-session-fixture.mjs';

test('existing Linux v7 fixture stays byte-for-byte identical', () => {
  const fixture = migrationSessionFixture();
  assert.equal(fixture.bytes, 708);
  assert.equal(fixture.sha256, 'f04ac0f666365c5819bda050ccbe5e298ccd532258262c1c151d2b98daaa0f7a');
  assert.equal(fixture.directory, 'sessions/--qa-test-fixture-workspace--/qa-valid-session');
});

test('macOS canonical QA cwd agrees with header, session directory and source kind', () => {
  const cwd = '/Volumes/fixture/repo/.codex-temp/macos-acceptance/fixture-workspace';
  const fixture = migrationSessionFixture(cwd);
  const [header, message, ...rest] = fixture.plaintext.trim().split('\n').map(JSON.parse);
  assert.equal(header.cwd, cwd);
  assert.equal(fixture.directory, `sessions/${frozenProjectKey(cwd)}/qa-valid-session`);
  assert.equal(message.data.source.kind, 'user');
  assert.equal(rest.length, 5);
  assert.notEqual(fixture.sha256, migrationSessionFixture().sha256);
});

test('frozen project keys preserve separator runs, UTF-16 escaping and truncation', () => {
  assert.equal(frozenProjectKey('/a//b\\c:d'), '--a-b-c-d--');
  assert.equal(frozenProjectKey('/a ~😀'), '--a~0020~007E~D83D~DE00--');
  assert.equal(frozenProjectKey('/'), '--root--');
  assert.equal(frozenProjectKey('/' + 'a'.repeat(260)).length, 255);
  assert.throws(() => frozenProjectKey(''));
});
