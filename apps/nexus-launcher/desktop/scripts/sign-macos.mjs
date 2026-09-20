import { readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import { inventoryResources, verifyInventory, releaseIdentity, verifyIdentity } from './prepare-release.mjs';

const sha = bytes => createHash('sha256').update(bytes).digest('hex');
async function defaultSign(options) {
  const require = createRequire(import.meta.url);
  const builder = createRequire(require.resolve('electron-builder'));
  await builder('app-builder-lib/out/codeSign/macCodeSign').sign(options);
}

// electron-builder calls this before notarization. Signing changes Mach-O bytes;
// update their manifests, then seal only the outer app with its original policy.
export async function signMacApplication(options, { sign = defaultSign, codesign = execFileSync } = {}) {
  if (!options.identity) throw new Error('macOS signing identity is required');
  const resources = path.join(options.app, 'Contents/Resources');
  const manifestFile = path.join(resources, 'release-manifest.json');
  const original = await readFile(manifestFile);
  const manifest = JSON.parse(original);
  verifyIdentity(manifest, JSON.parse(await readFile(path.join(resources, 'release-identity.json'))), sha(original));
  await verifyInventory(resources, manifest.files);
  await sign(options);
  const runtimeFile = path.join(resources, 'runtime/manifest.json');
  const runtime = JSON.parse(await readFile(runtimeFile));
  runtime.node.sha256 = sha(await readFile(path.join(resources, 'runtime/node/node')));
  await writeFile(runtimeFile, JSON.stringify(runtime, null, 2) + '\n');
  manifest.runtime = runtime;
  manifest.files = await inventoryResources(resources, ['nexus-agent', 'nexus-launcher', 'nexusctl', 'nexus-desktop-bridge', 'runtime', 'notices']);
  const bytes = JSON.stringify(manifest, null, 2) + '\n';
  await writeFile(manifestFile, bytes);
  await writeFile(path.join(resources, 'release-identity.json'), JSON.stringify(releaseIdentity(manifest, sha(bytes)), null, 2) + '\n');
  const args = ['--force', '--sign', options.identity, '--preserve-metadata=identifier,entitlements,requirements,flags,runtime', '--generate-entitlement-der'];
  if (options.identity !== '-') args.push('--timestamp');
  if (options.keychain) args.push('--keychain', options.keychain);
  codesign('/usr/bin/codesign', [...args, options.app], { timeout: 120000, stdio: 'inherit' });
  codesign('/usr/bin/codesign', ['--verify', '--deep', '--strict', options.app], { timeout: 30000, stdio: 'inherit' });
  await verifyInventory(resources, manifest.files);
}
