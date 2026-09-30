import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
const source = fs.readFileSync(new URL('../electron/prepare-harness-desktop.mjs', import.meta.url), 'utf8');
const resolution = source.match(/const target = .*;/)[0];
const call = source.slice(source.indexOf('  prepareDevelopmentProject({'), source.indexOf('  fs.writeFileSync(projectStamp'));
function invoke(resolver, prepare) {
  vm.runInNewContext(resolution + '\n' + call, { resolveDesktopBuildTarget: resolver, prepareDevelopmentProject: prepare,
    projectDir: 'fixture/project', root: 'fixture', dependencyDir: 'fixture/modules', path,
    metadata: { version: '0.2.0-rc.2' }, DESKTOP_HOST_PROTOCOL_VERSION: 1, nodeVersion: '24.20.0', pnpm: {version:'11.7.0'} });
}
test('legacy preparation retains options without a target', () => {
  invoke(undefined, options => { assert.equal(Object.hasOwn(options, 'target'), false); assert.equal(options.release.version,'0.2.0-rc.2'); });
});
test('preparation passes resolved target and does not swallow unsupported targets', () => {
  for (const target of ['win-x64','mac-arm64','mac-x64']) invoke(() => target, options => assert.equal(options.target,target));
  assert.throws(() => invoke(() => {throw Error('unsupported target');}, () => assert.fail()), /unsupported target/);
});
test('installed Harness target contract accepts the Nexus call', {skip: !process.env.NEXUS_TEST_HARNESS_ROOT}, async () => {
  const upstream = await import(pathToFileURL(path.join(process.env.NEXUS_TEST_HARNESS_ROOT,'apps/desktop/scripts/desktop-build-paths.mjs')));
  for (const [platform,arch] of [['win32','x64'],['darwin','arm64'],['darwin','x64']]) {
    invoke(() => upstream.resolveDesktopBuildTarget({},platform,arch), options => {
      assert.deepEqual(upstream.desktopTargetPlatform(options.target),{platform,arch});
    });
  }
});

test('launch supplies the prepared runtime to the real rc.2 runtime guard', {skip: !process.env.NEXUS_TEST_HARNESS_ROOT}, async () => {
  const app = path.join(process.env.NEXUS_TEST_HARNESS_ROOT,'apps/desktop');
  const worker=fs.readFileSync(new URL('../electron/harness-desktop-worker.mjs',import.meta.url),'utf8');
  const code=worker.slice(worker.indexOf('  const { resolveDesktopTargetBuildPaths } = await import'),worker.indexOf('  delete desktopEnv.ELECTRON_RUN_AS_NODE;'));
  const desktopEnv={DSH_DESKTOP_PRIMARY_RUNTIME_DIR:'stale-unrelated-runtime'};
  const AsyncFunction=Object.getPrototypeOf(async function(){}).constructor;
  await new AsyncFunction('app','path','pathToFileURL','env','desktopEnv','fs',code)(app,path,pathToFileURL,{},desktopEnv,fs);
  const paths=await import(pathToFileURL(path.join(app,'scripts/desktop-build-paths.mjs')));
  assert.equal(desktopEnv.DSH_DESKTOP_PRIMARY_RUNTIME_DIR,paths.developmentRuntimeDirectory({}));
  const main=fs.readFileSync(path.join(app,'src/main.ts'),'utf8');
  const guard=main.match(/function developmentPrimaryRuntime\(\): string \{[\s\S]*?\n\}/)[0].replace('(): string','()');
  const context={process:{env:desktopEnv}};
  assert.equal(vm.runInNewContext(guard+'\ndevelopmentPrimaryRuntime()',context),desktopEnv.DSH_DESKTOP_PRIMARY_RUNTIME_DIR);
  assert.throws(()=>vm.runInNewContext(guard+'\ndevelopmentPrimaryRuntime()',{process:{env:{}}}),/required for an unpackaged/);
});
