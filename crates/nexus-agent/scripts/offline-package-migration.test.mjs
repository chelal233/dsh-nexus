import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';
import { portableConfiguration, keepCredentials, relocateConfiguration } from './offline-package.mjs';

test('preserving credentials rejects lost parents and removed array identities instead of silently dropping them', () => {
  assert.throws(() => keepCredentials({}, { providers: { local: { apiKey: 'old-key' } } }), /Exclude that configuration/);
  assert.throws(() => keepCredentials({ providers: [] }, { providers: [{ name: 'local', apiKey: 'old-key' }] }), /current environment was preserved/);
  assert.throws(() => keepCredentials({ provider: null }, { provider: { token: 'old-token' } }), /Existing credentials/);
  assert.deepEqual(keepCredentials({ providers: [{ name: 'local', model: 'new-model' }] }, { providers: [{ name: 'local', model: 'old-model', apiKey: 'old-key' }] }), { providers: [{ name: 'local', model: 'new-model', apiKey: 'old-key' }] });
  assert.deepEqual(keepCredentials({ credentials: {} }, { credentials: { provider: { apiKey: 'old-key' } } }), { credentials: { provider: { apiKey: 'old-key' } } });
});

const exec = promisify(execFile), root = fileURLToPath(new URL('../../../', import.meta.url));
const helper = fileURLToPath(new URL('./offline-package.mjs', import.meta.url));
const runtime = path.join(root, 'apps/nexus-launcher/desktop/resources/runtime');
test('portable configuration removes recognized credentials and relocates only path boundaries', () => {
  const value = { apiKey: 'secret', nested: { password: 'pass', token: 'token', endpoint: 'https://u:p@example.com/api?token=hidden', root: 'C:\\source\\profiles\\web', other: 'C:\\source-other' } };
  const result = portableConfiguration(value, false, { HOME: 'C:\\source' });
  assert.deepEqual(result, { nested: { endpoint: 'https://example.com/api?token=', root: '__NEXUS_OFFLINE_HOME__/profiles/web', other: 'C:\\source-other' } });
  assert.equal(portableConfiguration(value, true).apiKey, 'secret');
});

test('typed normal settings survive credential filtering without allowing string credentials through', () => {
  const ordinary = { tokenLimit: 2048, tokenCount: 9, tokenBudget: 4096, tokenEnabled: true, passwordEnabled: false, passwordMinLength: 12 };
  assert.deepEqual(portableConfiguration({ ...ordinary, apiKey: 'secret', token: 'secret', tokenTimeout: 'secret' },false),ordinary);
  assert.deepEqual(keepCredentials({ tokenLimit: 8192 },{ tokenLimit: 1024, apiKey: 'keep' }),{tokenLimit:8192,apiKey:'keep'});
  assert.deepEqual(keepCredentials({}, { controls: ordinary }),{});
  assert.deepEqual(portableConfiguration({ tokenLimit: 'secret', passwordEnabled: 'secret' },false),{});
});

test('Windows retained paths relocate across case, namespace and UNC forms without changing opaque values', () => {
  for (const [from,input,to,expected] of [
    ['C:\\Home','c:\\HOME\\keys\\private.pem','D:\\New','D:/New/keys/private.pem'],
    ['\\\\?\\C:\\Home','C:/home/keys/private.pem','D:\\New','D:/New/keys/private.pem'],
    ['\\\\server\\share\\Home','\\\\?\\UNC\\SERVER\\SHARE\\home\\key','\\\\server\\share\\new','//server/share/new/key'],
  ]) {
    const value={ credentials:{privateKeyPath:input,apiKey:input},root:input,description:input,url:`https://host/?path=${input}`,otherPath:from+'-other'};
    const next=relocateConfiguration(value,from,to);
    assert.equal(next.credentials.privateKeyPath,expected);assert.equal(next.root,expected);
    assert.equal(next.credentials.apiKey,input);assert.equal(next.description,input);assert.equal(next.url,value.url);assert.equal(next.otherPath,value.otherPath);
    assert.equal(portableConfiguration({apiKey:input},true,{HOME:from}).apiKey,input);
  }
});

test('plural path fields relocate lists without rewriting opaque secrets or adjacent roots', () => {
  for (const key of ['paths', 'files', 'dirs', 'directories', 'searchPaths', 'config_files', 'data-dirs', 'roots', 'homes']) {
    const value = { nested: { [key]: ['C:/old/a', 'c:\\OLD\\b', 'C:/old-other/c'] }, apiKey: 'C:/old/secret' };
    assert.deepEqual(relocateConfiguration(value, 'C:/old', 'D:/new'), {
      nested: { [key]: ['D:/new/a', 'D:/new/b', 'C:/old-other/c'] }, apiKey: value.apiKey,
    });
  }
});

test('credential preservation checks both incoming and local types at every nesting level', () => {
  for (const [key, oldValue, newValue] of [
    ['tokenLimit', 1024, 'incoming-secret'], ['tokenEnabled', false, 'incoming-secret'],
    ['passwordMinLength', 8, { value: 'incoming-secret' }], ['tokenLimit', 'local-secret', 2048],
  ]) {
    const previous = { providers: [{ name: 'local', settings: { [key]: oldValue } }] };
    const incoming = { providers: [{ name: 'local', settings: { [key]: newValue } }] };
    assert.deepEqual(keepCredentials(incoming, previous), previous);
  }
  assert.deepEqual(keepCredentials({ tokenLimit: 4096, passwordEnabled: true }, { tokenLimit: 1024, passwordEnabled: false }), { tokenLimit: 4096, passwordEnabled: true });
});

test('preserved-value notices report changes without exposing keys or secrets', () => {
  const events=[];
  const value=keepCredentials({nested:{tokenLimit:'incoming-secret'},apiKey:'same'}, {nested:{tokenLimit:1024},apiKey:'same'}, (...args)=>events.push(args));
  assert.equal(value.nested.tokenLimit,1024);
  assert.deepEqual(events,[[]]);
  const unchanged=[];
  assert.deepEqual(keepCredentials({model:'new'}, {model:'old',apiKey:'local-secret'}, (...args)=>unchanged.push(args)), {model:'new',apiKey:'local-secret'});
  keepCredentials({apiKey:{b:2,a:1}}, {apiKey:{a:1,b:2}}, (...args)=>unchanged.push(args));
  keepCredentials({apiKey:'same'},{apiKey:'same'},(...args)=>unchanged.push(args));
  assert.deepEqual(unchanged,[]);
});

test('selected profiles and transitive plugin dependencies survive offline relocation', { skip: process.platform !== 'win32', timeout: 180000 }, async () => {
  await fs.access(path.join(runtime, 'node/node.exe'));
  const base = path.join(root, 'target-rtest/offline-migration-tests'); await fs.mkdir(base, { recursive: true });
  const work = await fs.mkdtemp(path.join(base, 'case-'));
  const slot = path.join(work, 'source-slot'), home = path.join(work, 'source-home');
  const put = async (name, text) => { await fs.mkdir(path.dirname(name), { recursive: true }); await fs.writeFile(name, text); };
  const link = async (target, name) => { await fs.mkdir(path.dirname(name), { recursive: true }); await fs.symlink(target, name, 'junction'); };
  try {
    await put(path.join(slot, 'apps/cli/lib/bin.js'), 'console.log("fixture")');
    await put(path.join(slot, 'packages/base/package.json'), JSON.stringify({ name: '@deepseek-ai/base', main: 'index.js' }));
    await put(path.join(slot, 'packages/base/index.js'), 'module.exports = "base";');
    const plugin = path.join(work, 'pool/.pnpm/plugin@1/node_modules/plugin');
    const dependency = path.join(work, 'pool/.pnpm/dependency@1/node_modules/dependency');
    await put(path.join(plugin, 'package.json'), JSON.stringify({ name: 'plugin', version: '1.0.0', main: 'index.js', bin: { 'plugin-cli': 'cli.js' } }));
    await put(path.join(plugin, 'index.js'), 'module.exports = require("dependency") + " plugin";');
    await put(path.join(plugin, 'cli.js'), 'console.log(require("./index.js"));');
    await put(path.join(dependency, 'package.json'), JSON.stringify({ name: 'dependency', version: '1.0.0', main: 'index.js' }));
    await put(path.join(dependency, 'index.js'), 'module.exports = "dependency";');
    await link(dependency, path.join(plugin, '../dependency'));
    for (const name of ['web', 'private']) {
      await put(path.join(home, `profiles/${name}/package.json`), JSON.stringify({ name, dependencies: { plugin: '1.0.0', '@deepseek-ai/base': '1.0.0' }, dsh: { profile: { bundles: ['@deepseek-ai/base', 'plugin'] } } }));
      await put(path.join(home, `profiles/${name}/cordis.patch.yml`), JSON.stringify({ apiKey: 'profile-secret', label: name, root: home }));
      await link(plugin, path.join(home, `profiles/${name}/node_modules/plugin`));
      await link(path.join(slot, 'packages/base'), path.join(home, `profiles/${name}/node_modules/@deepseek-ai/base`));
    }
    await put(path.join(home, 'settings.yaml'), 'apiKey: global-secret\nmodel: fixture-model\n');
    await put(path.join(home, '.env'), 'TOKEN=environment-secret');
    await put(path.join(home, '.credentials.yaml'), 'accounts: managed-secret');
    await put(path.join(home, 'sessions/business.txt'), 'must never be exported');
    const archive = path.join(work, 'environment.tar.gz');
    async function run(action, extra = {}, runId = action) {
      const directory = path.join(work, runId); await fs.mkdir(directory, { recursive: true });
      const job = { action, id: action, work: directory, tools: runtime, git_runtime: path.join(runtime, "git"), archive, slot, runtime, home, version: 'fixture', active_profile: 'web', nexus: {}, private_writer: path.join(process.env.CARGO_TARGET_DIR || path.join(root, 'target'), 'debug/nexus-agent.exe'), ...extra };
      const file = path.join(directory, 'job.json'); await fs.writeFile(file, JSON.stringify(job));
      await exec(process.execPath, [helper, file], { windowsHide: true, timeout: 120000 });
      return directory;
    }
    const exported = await run('export', { contents: { profiles: ['web'], configuration: true, plugins: true, credentials: false } });
    const preview = await run('inspect'); const summary = JSON.parse(await fs.readFile(path.join(preview, 'result.json')));
    assert.deepEqual(summary.contents.profiles, ['web']); assert.equal(summary.contents.credentials, false);
    const credentials = await run('export', { archive: path.join(work, 'with-credentials.tar.gz'), contents: { profiles: ['web'], configuration: true, plugins: false, credentials: true } }, 'credentials');
    const credentialHome = path.join(credentials, 'payload/environment');
    assert.equal(JSON.parse(await fs.readFile(path.join(credentialHome, 'settings.yaml'))).apiKey, 'global-secret');
    assert.equal(await fs.readFile(path.join(credentialHome, '.env'), 'utf8'), 'TOKEN=environment-secret');
    assert.deepEqual(JSON.parse(await fs.readFile(path.join(credentialHome, 'profiles/web/package.json'))).dsh.profile.bundles, ['@deepseek-ai/base']);
    await assert.rejects(fs.access(path.join(credentialHome, 'profiles/web/node_modules/plugin')));
    assert.equal(await fs.readFile(path.join(credentialHome, '.credentials.yaml'), 'utf8'), 'accounts: managed-secret');
    const dataArchive = path.join(work, 'data-only.tar.gz');
    const data = await run('export', { archive: dataArchive, contents: { runtime: false, profiles: ['web', 'private'], configuration: true, environment: true, sessions: true, plugins: true, credentials: true } }, 'data-export');
    const dataManifest = JSON.parse(await fs.readFile(path.join(data, 'payload/manifest.json')));
    const legacyImport = await run('import', { archive: dataArchive }, 'legacy-client-defaults');
    assert.equal(JSON.parse(await fs.readFile(path.join(legacyImport, 'result.json'))).contents.credentials, false);
    await assert.rejects(fs.access(path.join(legacyImport, 'payload/environment/.env')));
    assert.equal(JSON.parse(await fs.readFile(path.join(legacyImport, 'payload/environment/settings.yaml'))).apiKey, undefined);
    for (const plugins of [false, true]) for (const configuration of [false, true]) for (const credentials of [false, true]) {
      const id = `matrix-${+plugins}${+configuration}${+credentials}`;
      const receiver = path.join(work, `${id}-home`), destination = path.join(work, `${id}-destination`);
      const original = `apiKey: local-secret\nlabel: local-label\ntokenLimit: 4000\ncredentials:\n  privateKeyPath: ${JSON.stringify(receiver.toUpperCase() + '/keys/local.pem')}\n  apiKey: ${JSON.stringify(receiver.toUpperCase() + '/opaque-key')}\n`;
      await put(path.join(receiver, 'profiles/web/package.json'), JSON.stringify({ name: 'web', dependencies: { local: '2.0.0' } }));
      await put(path.join(receiver, 'profiles/web/cordis.patch.yml'), original);
      await put(path.join(receiver, 'profiles/web/node_modules/local/package.json'), '{"name":"local"}');
      const staged = await run('import', { archive: dataArchive, contents: { runtime: false, profiles: ['web'], configuration, plugins, credentials, credential_policy: 'replace', environment: false, sessions: false } }, id);
      await run('merge_environment', { work: staged, home: receiver, environment: destination, slot:null }, `${id}-merge`);
      await fs.rename(path.join(staged, 'payload/merged-environment'), destination);
      await run('finalize', { work: staged, environment: destination }, `${id}-finalize`);
      const patch = JSON.parse(await fs.readFile(path.join(destination, 'profiles/web/cordis.patch.yml')));
      assert.equal(patch.label, configuration ? 'web' : 'local-label', id);
      assert.equal(patch.apiKey, configuration && credentials ? 'profile-secret' : 'local-secret', id);
      if (!configuration || !credentials) {
        assert.equal(patch.credentials.privateKeyPath,destination.replaceAll('\\','/')+'/keys/local.pem',id);
        assert.equal(patch.credentials.apiKey,receiver.toUpperCase()+'/opaque-key',id);
      }
      assert.equal(await fs.readFile(path.join(receiver, 'profiles/web/cordis.patch.yml'), 'utf8'), original, id);
      const pkg = JSON.parse(await fs.readFile(path.join(destination, 'profiles/web/package.json')));
      assert.equal(pkg.dependencies.local, plugins ? undefined : '2.0.0', id);
      if (plugins) await assert.rejects(fs.access(path.join(destination, 'profiles/web/node_modules/local')));
      else await fs.access(path.join(destination, 'profiles/web/node_modules/local/package.json'));
    }
    assert.equal(dataManifest.versions, null);
    for (const policy of [undefined, 'preserve', 'replace']) {
      const id = `credential-policy-${policy || 'default'}`;
      const receiver = path.join(work, `${id}-home`), destination = path.join(work, `${id}-destination`);
      await put(path.join(receiver, 'profiles/web/package.json'), '{"name":"web"}');
      await put(path.join(receiver, 'profiles/web/cordis.patch.yml'), '{"apiKey":"local-profile-key","label":"local"}');
      await put(path.join(receiver, 'settings.yaml'), '{"apiKey":"local-global-key","model":"local"}');
      await put(path.join(receiver, '.env'), 'TOKEN=local-token');
      await put(path.join(receiver, '.credentials.yaml'), 'accounts: local-account');
      const selected = await run('import', { archive: dataArchive, contents: { runtime: false, profiles: ['web'], configuration: true, plugins: false, environment: true, sessions: false, credentials: true, ...(policy ? { credential_policy: policy } : {}) } }, id);
      await run('merge_environment', { work: selected, home: receiver, environment: destination }, `${id}-merge`);
      const summary = JSON.parse(await fs.readFile(path.join(selected, 'merge-summary.json')));
      assert.deepEqual(Object.keys(summary).sort(), ['preserved_configuration_values', 'schema_version']);
      assert.equal(summary.preserved_configuration_values > 0, policy !== 'replace');
      await fs.rename(path.join(selected, 'payload/merged-environment'), destination);
      await run('finalize', { work: selected, environment: destination }, `${id}-finalize`);
      const replace = policy === 'replace';
      assert.equal(await fs.readFile(path.join(destination, '.env'), 'utf8'), replace ? 'TOKEN=environment-secret' : 'TOKEN=local-token');
      assert.equal(await fs.readFile(path.join(destination, '.credentials.yaml'), 'utf8'), replace ? 'accounts: managed-secret' : 'accounts: local-account');
      assert.equal(JSON.parse(await fs.readFile(path.join(destination, 'settings.yaml'))).apiKey, replace ? 'global-secret' : 'local-global-key');
      assert.equal(JSON.parse(await fs.readFile(path.join(destination, 'profiles/web/cordis.patch.yml'))).apiKey, replace ? 'profile-secret' : 'local-profile-key');
      assert.equal(await fs.readFile(path.join(receiver, '.env'), 'utf8'), 'TOKEN=local-token');
      const recordPath = path.join(selected, 'credential-recovery.json');
      if (replace) {
        const text = await fs.readFile(recordPath, 'utf8'), record = JSON.parse(text);
        assert.equal(record.previous_home, await fs.realpath(receiver)); assert.equal(record.imported_home, destination);
        assert.ok(record.files.includes('.env')); assert.ok(record.files.includes('profiles/web/cordis.patch.yml'));
        assert.doesNotMatch(text, /local-token|local-account|local-profile-key|local-global-key/);
      } else await assert.rejects(fs.access(recordPath));
    }
    assert.ok(dataManifest.entries.every(entry => entry.path.startsWith('environment')));
    const selected = await run('import', { archive: dataArchive, contents: { runtime: false, profiles: ['web'], configuration: true, environment: false, sessions: false, plugins: false, credentials: false } }, 'profile-subset');
    const selectedHome = path.join(selected, 'payload/environment');
    assert.equal(JSON.parse(await fs.readFile(path.join(selectedHome, 'profiles/web/cordis.patch.yml'))).apiKey, undefined);
    assert.equal(JSON.parse(await fs.readFile(path.join(selectedHome, 'profiles/web/package.json'))).dependencies.plugin, undefined);
    for (const name of ['profiles/private', 'profiles/web/node_modules', 'sessions', 'settings.yaml', '.env', '.credentials.yaml', '.packages']) await assert.rejects(fs.access(path.join(selectedHome, name)));
    const redirected = path.join(work, 'redirected-recipient'), outside = path.join(work, 'outside-profiles');
    await put(path.join(outside, 'web/package.json'), '{"name":"must-stay"}');
    await link(outside, path.join(redirected, 'profiles'));
    await assert.rejects(run('merge_environment', { work: selected, home: redirected, environment: path.join(work, 'rejected-home') }, 'reject-redirected-profile'));
    assert.equal(await fs.readFile(path.join(outside, 'web/package.json'), 'utf8'), '{"name":"must-stay"}');
    const preserved = await run('import', { archive: dataArchive, contents: { runtime: false, profiles: ['web'], configuration: true, environment: true, sessions: false, plugins: false, credentials: false } }, 'preserve-local-selection');
    const localHome = path.join(work, 'local-profile-recipient'), localDestination = path.join(work, 'local-profile-destination');
    await put(path.join(localHome, 'profiles/web/package.json'), JSON.stringify({ name: 'web', dependencies: { local: '2.0.0' }, dsh: { profile: { bundles: ['local'] } } }));
    await put(path.join(localHome, 'profiles/web/node_modules/local/package.json'), '{"name":"local","version":"2.0.0","main":"index.js"}');
    await put(path.join(localHome, 'profiles/web/node_modules/local/index.js'), 'module.exports = "kept local plugin";');
    await put(path.join(localHome, 'profiles/web/cordis.patch.yml'), '{"apiKey":"receiver-profile-secret","label":"old-label"}');
    await put(path.join(localHome, 'settings.yaml'), '{"apiKey":"receiver-global-secret","model":"old-model"}');
    await run('merge_environment', { work: preserved, home: localHome, environment: localDestination }, 'preserve-local-merge');
    const mergeSummary=JSON.parse(await fs.readFile(path.join(preserved,'merge-summary.json')));
    assert.equal(mergeSummary.preserved_configuration_values,0);
    assert.deepEqual(Object.keys(mergeSummary).sort(),['preserved_configuration_values','schema_version']);
    await fs.rename(path.join(preserved, 'payload/merged-environment'), localDestination);
    await run('finalize', { work: preserved, environment: localDestination }, 'preserve-local-finalize');
    const localManifest = JSON.parse(await fs.readFile(path.join(localDestination, 'profiles/web/package.json')));
    assert.deepEqual(localManifest.dependencies, { local: '2.0.0' });
    assert.deepEqual(localManifest.dsh.profile.bundles, ['local']);
    const localPatch = JSON.parse(await fs.readFile(path.join(localDestination, 'profiles/web/cordis.patch.yml')));
    assert.equal(localPatch.apiKey, 'receiver-profile-secret'); assert.equal(localPatch.label, 'web');
    assert.deepEqual(JSON.parse(await fs.readFile(path.join(localDestination, 'settings.yaml'))), { model: 'fixture-model', apiKey: 'receiver-global-secret' });
    await fs.rename(localHome, path.join(work, 'hidden-local-recipient'));
    const localLoaded = await exec(path.join(runtime, 'node/node.exe'), ['-e', 'console.log(require("local"))'], { cwd: path.join(localDestination, 'profiles/web'), windowsHide: true });
    assert.equal(localLoaded.stdout.trim(), 'kept local plugin');
    const pluginImport = await run('import', { archive: dataArchive, contents: { runtime: false, profiles: ['web'], configuration: true, environment: false, sessions: false, plugins: true, credentials: false } }, 'plugin-overlay-selection');
    const sharedHome = path.join(work, 'shared-recipient'), sharedDestination = path.join(work, 'shared-destination');
    const oldUnit = dataManifest.links.find(link => link.path === 'environment/profiles/web/node_modules/plugin').target.replace(/^environment\//, '');
    await put(path.join(sharedHome, oldUnit, 'package.json'), '{"name":"plugin","version":"0.5.0","main":"index.js"}');
    await put(path.join(sharedHome, oldUnit, 'index.js'), 'module.exports = "receiver plugin";');
    await put(path.join(sharedHome, 'profiles/other/package.json'), '{"name":"other"}');
    await link(path.join(sharedHome, oldUnit), path.join(sharedHome, 'profiles/other/node_modules/plugin'));
    await run('merge_environment', { work: pluginImport, home: sharedHome, environment: sharedDestination }, 'plugin-overlay-merge');
    await fs.rename(path.join(pluginImport, 'payload/merged-environment'), sharedDestination);
    await run('finalize', { work: pluginImport, environment: sharedDestination }, 'plugin-overlay-finalize');
    await fs.rename(sharedHome, path.join(work, 'hidden-shared-recipient'));
    const oldLoaded = await exec(path.join(runtime, 'node/node.exe'), ['-e', 'console.log(require("plugin"))'], { cwd: path.join(sharedDestination, 'profiles/other'), windowsHide: true });
    const newLoaded = await exec(path.join(runtime, 'node/node.exe'), ['-e', 'console.log(require("plugin"))'], { cwd: path.join(sharedDestination, 'profiles/web'), windowsHide: true });
    assert.equal(oldLoaded.stdout.trim(), 'receiver plugin');
    assert.equal(newLoaded.stdout.trim(), 'dependency plugin');
    const sessionOnly = await run('import', { archive: dataArchive, contents: { runtime: false, profiles: [], configuration: false, environment: false, sessions: true, plugins: false, credentials: false } }, 'session-subset');
    const recipient = path.join(work, 'recipient'), destination = path.join(work, 'independent-environment');
    await put(path.join(recipient, 'profiles/local/package.json'), '{"name":"local"}');
    await put(path.join(recipient, '.env'), 'LOCAL_ACCOUNT=keep');
    await put(path.join(recipient, 'sessions/local.txt'), 'keep local history');
    await run('merge_environment', { work: sessionOnly, home: recipient, environment: destination }, 'session-merge');
    await fs.rename(path.join(sessionOnly, 'payload/merged-environment'), destination);
    await run('finalize', { work: sessionOnly, environment: destination, runtime: path.join(work, 'no-runtime'), slot: path.join(work, 'no-slot') }, 'session-finalize');
    assert.equal(await fs.readFile(path.join(destination, 'sessions/business.txt'), 'utf8'), 'must never be exported');
    assert.equal(await fs.readFile(path.join(destination, 'sessions/local.txt'), 'utf8'), 'keep local history');
    assert.equal(await fs.readFile(path.join(destination, '.env'), 'utf8'), 'LOCAL_ACCOUNT=keep');
    await fs.access(path.join(destination, 'profiles/local/package.json'));
    await assert.rejects(run('import', { archive: dataArchive, contents: { runtime: true, profiles: [], configuration: false, environment: false, sessions: false, plugins: false, credentials: false } }, 'unavailable-component'));
    const imported = await run('import');
    const importedSlot = path.join(imported, 'payload/slot'), importedRuntime = path.join(imported, 'payload/runtime');
    await fs.rename(path.join(imported, 'payload/environment'), path.join(importedRuntime, 'environment'));
    await run('finalize', { work: imported, slot: importedSlot, runtime: importedRuntime });
    const importedGit=await exec(path.join(importedRuntime,'git/cmd/git.exe'),['--version'],{windowsHide:true,env:{...process.env,PATH:''}});
    assert.match(importedGit.stdout,/git version/);
    const importedHome = path.join(importedRuntime, 'environment');
    const config = JSON.parse(await fs.readFile(path.join(importedHome, 'settings.yaml')));
    assert.equal(config.apiKey, undefined); assert.equal(config.model, 'fixture-model');
    const patch = JSON.parse(await fs.readFile(path.join(importedHome, 'profiles/web/cordis.patch.yml')));
    assert.equal(patch.apiKey, undefined); assert.equal(patch.root.replaceAll('\\', '/'), importedHome.replaceAll('\\', '/'));
    await assert.rejects(fs.access(path.join(importedHome, '.env')));
    await assert.rejects(fs.access(path.join(importedHome, 'profiles/private')));
    await assert.rejects(fs.access(path.join(importedHome, 'sessions')));
    // Remove the entire source graph so success cannot accidentally resolve it.
    const movedHome = path.join(work, 'hidden-home'); await fs.rename(home, movedHome);
    await fs.rename(slot, path.join(work, 'hidden-slot')); await fs.rename(path.join(work, 'pool'), path.join(work, 'hidden-pool'));
    const probe = await exec(process.execPath, ['-e', 'console.log(require("plugin")); console.log(require("@deepseek-ai/base"));'], { cwd: path.join(importedHome, 'profiles/web'), windowsHide: true });
    assert.match(probe.stdout, /dependency plugin/); assert.match(probe.stdout, /base/);
    const progress = JSON.parse(await fs.readFile(path.join(exported, 'progress.json'))); assert.equal(progress.stage, 'flush_archive');
  } finally {
    assert.ok(path.resolve(work).startsWith(path.resolve(base) + path.sep));
    await fs.rm(work, { recursive: true, force: true });
  }
});
