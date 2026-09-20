import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';

export const digest = file => createHash('sha256').update(fs.readFileSync(file)).digest('hex');
export function verifyDesktopKit(directory, { platform = process.platform, arch = process.arch } = {}) {
  directory = path.resolve(directory);
  const manifest = JSON.parse(fs.readFileSync(path.join(directory, 'manifest.json'), 'utf8'));
  if (![1,2,3].includes(manifest.schema) || manifest.platform !== platform || manifest.arch !== arch ||
      !['win32','darwin','linux'].includes(platform) || !Array.isArray(manifest.files) || !manifest.files.length) throw new Error('desktop_runtime_incompatible');
  const seen = new Set();
  const hashes = new Map();
  for (const entry of manifest.files) {
    if (typeof entry.path !== 'string' || entry.path.includes('\\') || entry.path.includes(':') ||
        entry.path.split('/').some(part => !part || part === '.' || part === '..') || seen.has(entry.path)) throw new Error('desktop_runtime_invalid');
    seen.add(entry.path);
    const file = path.join(directory, entry.path);
    for (let parent = path.dirname(file); parent !== path.resolve(directory); parent = path.dirname(parent)) {
      if (parent === path.dirname(parent) || fs.lstatSync(parent).isSymbolicLink()) throw new Error('desktop_runtime_invalid');
    }
    if (!fs.lstatSync(file).isFile()) throw new Error('desktop_runtime_invalid');
    const hash = digest(file);
    if (hash !== entry.sha256) throw new Error('desktop_runtime_invalid');
    hashes.set(entry.path, hash);
  }
  if (!seen.has('lock.json') || hashes.get('lock.json') !== manifest.lockSha256) throw new Error('desktop_runtime_invalid');
  const lock = JSON.parse(fs.readFileSync(path.join(directory, 'lock.json'), 'utf8'));
  const target = lock.targets[`${({ win32: 'win', darwin: 'mac', linux: 'linux' })[platform]}-${arch}`];
  if (manifest.schema >= 2 && manifest.supported === false) {
    if (target || seen.size !== 1 || manifest.reason !== 'upstream_target_unsupported') throw new Error('desktop_runtime_invalid');
    return manifest;
  }
  if (!target) throw new Error('desktop_runtime_incompatible');
  if (manifest.schema === 3) {
    if (manifest.supported !== true || manifest.electronMode !== 'launcher' ||
        !seen.has('primary.tar.gz') || hashes.get('primary.tar.gz') !== manifest.primaryArchiveSha256 ||
        !/^[0-9]+\.[0-9]+\.[0-9]+$/.test(manifest.electronVersion) ||
        !/^[0-9]+\.[0-9]+\.[0-9]+$/.test(manifest.electronNodeVersion) || manifest.primarySmokePassed !== true) throw new Error('desktop_runtime_invalid');
    if (manifest.hostArchiveSha256 !== undefined && (!['win32', 'darwin'].includes(platform) || !seen.has('host.tar.gz') ||
        hashes.get('host.tar.gz') !== manifest.hostArchiveSha256)) throw new Error('desktop_runtime_invalid');
    return { ...manifest, primaryArchive: path.join(directory, 'primary.tar.gz'),
      ...(manifest.hostArchiveSha256 ? { hostArchive: path.join(directory, 'host.tar.gz') } : {}) };
  }
  for (const hash of [target.nodeSha256, target.pythonSha256, ...target.wheels.map(item => item.sha256), ...lock.wheels.map(item => item.sha256)]) {
    if (!/^[a-f0-9]{64}$/.test(hash) || !seen.has(`assets/${hash}`)) throw new Error('desktop_runtime_missing');
  }
  if (manifest.schema === 2) {
    if (manifest.supported !== true || !seen.has('electron.zip') || hashes.get('electron.zip') !== manifest.electronArchiveSha256) throw new Error('desktop_runtime_invalid');
    return { ...manifest, archive: path.join(directory, 'electron.zip') };
  }
  const electron = platform === 'win32' ? 'electron/electron.exe' : 'electron/Electron.app/Contents/MacOS/Electron';
  if (!seen.has(electron)) throw new Error('desktop_runtime_invalid');
  return { ...manifest, electron: path.join(directory, electron) };
}

function electronInventory(root) {
  root = fs.realpathSync(root); // macOS /var and /private/var can name the same cache.
  const files = [];
  function visit(relative) {
    const file = path.join(root, relative), stat = fs.lstatSync(file);
    if (stat.isSymbolicLink()) {
      const link = fs.readlinkSync(file), resolved = fs.realpathSync(file);
      const within = path.relative(root, resolved);
      if (path.isAbsolute(link) || within.startsWith('..') || path.isAbsolute(within)) throw new Error('desktop_runtime_invalid');
      files.push({ path: relative, link });
    } else if (stat.isDirectory()) for (const name of fs.readdirSync(file).sort()) visit(path.join(relative, name));
    else if (stat.isFile()) files.push({ path: relative, sha256: digest(file), mode: stat.mode & 0o777 });
    else throw new Error('desktop_runtime_invalid');
  }
  visit(''); return files;
}

// Only extracts a verified local archive. Keeps macOS framework links and mode
// bits intact, and rechecks cached bytes before reusing an extracted runtime.
export function prepareDesktopElectron(directory, cache) {
  const kit = verifyDesktopKit(directory);
  if (kit.supported === false) throw new Error('desktop_unsupported');
  if (kit.schema === 3) throw new Error('desktop_requires_launcher_runtime');
  if (kit.electron) return kit;
  cache = path.resolve(cache);
  fs.mkdirSync(cache, { recursive: true });
  const destination = path.join(cache, `electron-${kit.electronArchiveSha256}`);
  const stamp = `${destination}.json`;
  const executable = legacyElectronEntry();
  let valid = false;
  try { valid = !fs.lstatSync(destination).isSymbolicLink() && JSON.stringify(electronInventory(destination)) === fs.readFileSync(stamp, 'utf8'); } catch {}
  if (!valid) {
    const temporary = fs.mkdtempSync(path.join(cache, `.electron-${process.pid}-`));
    try {
      if (process.platform === 'win32') execFileSync(path.join(process.env.SystemRoot || 'C:\\Windows', 'System32/tar.exe'), ['-xf', kit.archive, '-C', temporary], { windowsHide: true, timeout: 120000 });
      else execFileSync('/usr/bin/ditto', ['-x', '-k', kit.archive, temporary], { timeout: 120000 });
      if (!fs.statSync(path.join(temporary, executable)).isFile()) throw new Error('desktop_runtime_invalid');
      const files = electronInventory(temporary);
      if (fs.existsSync(destination)) fs.rmSync(destination, { recursive: true });
      fs.renameSync(temporary, destination);
      fs.writeFileSync(stamp, JSON.stringify(files));
    } finally { fs.rmSync(temporary, { recursive: true, force: true }); }
  }
  return { ...kit, electron: path.join(destination, executable) };
}

export function selectDesktopKit(resources, config) {
  const node = config.runtime?.node;
  const imported = node?.ownership === 'nexus' && typeof node.path === 'string'
    ? path.join(path.dirname(path.dirname(node.path)), 'desktop') : undefined;
  const candidates = [imported, path.join(resources, 'runtime/desktop')].filter(Boolean);
  const directory = candidates.find(item => fs.existsSync(path.join(item, 'manifest.json')));
  if (!directory) throw new Error('desktop_runtime_missing');
  return directory;
}

export const legacyElectronEntry = (platform = process.platform) => platform === 'win32' ? 'electron.exe' : 'Electron.app/Contents/MacOS/Electron';
export const portableHostEntry = (platform = process.platform) => platform === 'win32' ? 'Nexus Launcher.exe' : 'Nexus Launcher.app/Contents/MacOS/Nexus Launcher';
