// Build-time downloads only. Launch and offline import never call this script.
import fs from 'node:fs';
import path from 'node:path';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import os from 'node:os';
import { nativeTar } from '../../electron/desktop-runtime-cache.mjs';
import { digest, verifyDesktopKit } from '../../electron/desktop-runtime.mjs';
import { selectPlatform } from './release-platform.mjs';

export const pin = JSON.parse(fs.readFileSync(new URL('../desktop-runtime-lock.json', import.meta.url), 'utf8'));
const require = createRequire(import.meta.url);
const builderRequire = createRequire(require.resolve('electron-builder'));
const packagerRequire = createRequire(builderRequire.resolve('app-builder-lib'));
const targetName = (platform, arch) => `${({ win32: 'win', darwin: 'mac', linux: 'linux' })[platform]}-${arch}`;

export function desktopAssets(lock, platform, arch) {
  const target = lock.targets[targetName(platform, arch)];
  if (!target) return null;
  const python = `cpython-${lock.pythonVersion}+${lock.pythonRelease}-${target.pythonTarget}-install_only_stripped.tar.gz`;
  return [
    { url: `https://nodejs.org/dist/v${lock.nodeVersion}/node-v${lock.nodeVersion}-${target.nodeArchive}`, sha256: target.nodeSha256 },
    { url: `https://github.com/astral-sh/python-build-standalone/releases/download/${lock.pythonRelease}/${encodeURIComponent(python)}`, sha256: target.pythonSha256 },
    ...target.wheels, ...lock.wheels,
  ];
}

export async function cachedAsset(url, hash, cache, fetcher = fetch) {
  if (!/^[a-f0-9]{64}$/.test(hash) || !url.startsWith('https://')) throw new Error('Invalid pinned Desktop asset');
  fs.mkdirSync(cache, { recursive: true });
  const file = path.join(cache, hash);
  if (fs.existsSync(file) && digest(file) === hash) return file;
  const temporary = `${file}.${process.pid}.tmp`;
  try {
    const response = await fetcher(url, { signal: AbortSignal.timeout(180000) });
    if (!response.ok || !response.body) throw new Error(`Desktop download failed (${response.status}): ${url}`);
    await pipeline(Readable.fromWeb(response.body), fs.createWriteStream(temporary));
    if (digest(temporary) !== hash) throw new Error(`Desktop checksum mismatch: ${url}`);
    fs.renameSync(temporary, file);
    return file;
  } finally { fs.rmSync(temporary, { force: true }); }
}

export async function stageDesktopRuntime({ output, cache, source, platform = process.platform, arch = process.arch, fetcher = fetch }) {
  selectPlatform(undefined, platform, arch);
  output = path.resolve(output); cache = path.resolve(cache);
  if (source && !path.isAbsolute(source)) throw new Error('Desktop source must be absolute');
  const localLock = source && path.join(source, 'apps/desktop/scripts/primary-runtime-lock.json');
  const lockFile = localLock || await cachedAsset(`https://raw.githubusercontent.com/${pin.repository}/${pin.commit}/apps/desktop/scripts/primary-runtime-lock.json`, pin.lockSha256, cache, fetcher);
  if (digest(lockFile) !== pin.lockSha256) throw new Error('Desktop source does not match the pinned release');
  const lock = JSON.parse(fs.readFileSync(lockFile, 'utf8'));
  const assets = desktopAssets(lock, platform, arch);
  if (assets && (platform !== process.platform || arch !== process.arch)) throw new Error('Build and smoke Desktop on its native platform');
  const versions = assets ? JSON.parse(execFileSync(require('electron'), ['-p', 'JSON.stringify(process.versions)'],
    { encoding: 'utf8', windowsHide: true, env: { ...process.env, ELECTRON_RUN_AS_NODE: '1' }, timeout: 15000 })) : null;
  if (versions && versions.electron !== pin.electronVersion) throw new Error('Installed Electron does not match the Desktop pin');
  fs.mkdirSync(path.dirname(output), { recursive: true });
  if (fs.existsSync(output) && fs.lstatSync(output).isSymbolicLink()) throw new Error('Linked Desktop output');
  const stage = fs.mkdtempSync(path.join(path.dirname(output), '.desktop-stage-'));
  try {
    fs.copyFileSync(lockFile, path.join(stage, 'lock.json'));
    const files = [{ path: 'lock.json', sha256: pin.lockSha256 }];
    if (assets) {
      fs.mkdirSync(path.join(stage, 'assets'));
      for (const asset of assets) {
        const prepared = source && path.join(source, 'apps/desktop/.desktop-build/downloads', asset.sha256);
        const file = prepared && fs.existsSync(prepared) && digest(prepared) === asset.sha256
          ? prepared : await cachedAsset(asset.url, asset.sha256, cache, fetcher);
        fs.copyFileSync(file, path.join(stage, 'assets', asset.sha256));
        files.push({ path: `assets/${asset.sha256}`, sha256: asset.sha256 });
      }
      const sourceArchive = await cachedAsset(`https://codeload.github.com/${pin.repository}/tar.gz/${pin.commit}`, pin.sourceArchiveSha256, cache, fetcher);
      const work = fs.mkdtempSync(path.join(os.tmpdir(), 'nexus-primary-build-'));
      try {
        execFileSync(nativeTar(), ['-xf', sourceArchive, '-C', work, '--strip-components=1'], { windowsHide: true, timeout: 120000 });
        const app = path.join(work, 'apps/desktop'), modules = path.join(app, 'node_modules');
        fs.mkdirSync(modules, { recursive: true });
        const link = (from, to) => { fs.mkdirSync(path.dirname(to), { recursive: true }); fs.symlinkSync(fs.realpathSync(from), to, process.platform === 'win32' ? 'junction' : 'dir'); };
        link(path.dirname(require.resolve('extract-zip/package.json')), path.join(modules, 'extract-zip'));
        link(path.dirname(packagerRequire.resolve('tar/package.json')), path.join(modules, 'tar'));
        link(path.dirname(builderRequire.resolve('app-builder-lib/package.json')), path.join(modules, 'app-builder-lib'));
        link(fileURLToPath(new URL('../resources/runtime/pnpm', import.meta.url)), path.join(modules, 'pnpm'));
        link(path.join(work, 'packages/skill/skill-office'), path.join(work, 'apps/desktop-host/node_modules/@deepseek-ai/dsh-skill-office'));
        execFileSync(process.execPath, [fileURLToPath(new URL('./assemble-desktop-primary.mjs', import.meta.url)), work, path.join(stage, 'assets')],
          { stdio: 'inherit', windowsHide: true, timeout: 600000, env: { ...process.env, PYTHONDONTWRITEBYTECODE: '1' } });
        const runtime = path.join(app, '.desktop-build/targets', targetName(platform, arch), 'runtime');
        execFileSync(nativeTar(), ['-czf', path.join(stage, 'primary.tar.gz'), '-C', runtime, 'primary-runtime', 'office-skills'], { windowsHide: true, timeout: 180000 });
      } finally { fs.rmSync(work, { recursive: true, force: true }); }
      fs.rmSync(path.join(stage, 'assets'), { recursive: true });
      files.splice(1);
      files.push({ path: 'primary.tar.gz', sha256: digest(path.join(stage, 'primary.tar.gz')) });
    }
    const manifest = { schema: 3, platform, arch, supported: !!assets, source: { repository: pin.repository, commit: pin.commit, version: pin.version },
      lockSha256: pin.lockSha256, electronVersion: pin.electronVersion, pnpmVersion: pin.pnpmVersion,
      ...(assets ? { electronMode: 'launcher', electronNodeVersion: versions.node, retiredElectronArchiveSha256: pin.electronArchives[`${platform}-${arch}`], primaryArchiveSha256: files[1].sha256, primarySmokePassed: true } : { reason: 'upstream_target_unsupported' }), files };
    fs.writeFileSync(path.join(stage, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
    verifyDesktopKit(stage, { platform, arch });
    const backup = `${output}.previous-${process.pid}`;
    if (fs.existsSync(backup)) throw new Error('Desktop staging backup already exists');
    const existed = fs.existsSync(output);
    if (existed) fs.renameSync(output, backup);
    try { fs.renameSync(stage, output); }
    catch (error) { if (existed) fs.renameSync(backup, output); throw error; }
    if (existed) fs.rmSync(backup, { recursive: true });
    console.log(`Desktop offline kit: ${platform}/${arch}, ${assets ? 'verified' : 'upstream does not support Desktop; Web remains available'}`);
    return manifest;
  } finally { fs.rmSync(stage, { recursive: true, force: true }); }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  selectPlatform(process.env.CARGO_BUILD_TARGET);
  await stageDesktopRuntime({
    output: process.env.NEXUS_DESKTOP_RESOURCE_DIR || fileURLToPath(new URL('../resources/runtime/desktop', import.meta.url)),
    cache: fileURLToPath(new URL('../../../../target/desktop-runtime-cache', import.meta.url)),
    source: process.env.NEXUS_HARNESS_DESKTOP_SOURCE,
  });
}
