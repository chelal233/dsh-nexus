import fs from 'node:fs';
import path from 'node:path';
import { portableHostEntry } from './desktop-runtime.mjs';
import { execFileSync } from 'node:child_process';

export const nativeTar = () => process.platform === 'win32'
  ? path.join(process.env.SystemRoot || 'C:\\Windows', 'System32/tar.exe') : '/usr/bin/tar';

// Include every entry; link targets cannot escape the owned tree. Metadata
// changes invalidate the cache, including same-size edits with restored mtime.
export function runtimeInventory(directory) {
  const root = fs.realpathSync(directory), entries = [];
  function visit(relative) {
    const file = path.join(root, relative), stat = fs.lstatSync(file, { bigint: true });
    if (stat.isSymbolicLink()) {
      const target = fs.realpathSync(file), within = path.relative(root, target);
      if (within === '..' || within.startsWith(`..${path.sep}`) || path.isAbsolute(within)) throw Error('desktop_runtime_invalid');
      entries.push([relative, 'link', fs.readlinkSync(file), target]);
    } else if (stat.isDirectory()) {
      entries.push([relative, 'directory']);
      for (const name of fs.readdirSync(file).sort()) visit(path.join(relative, name));
    } else if (stat.isFile()) entries.push([relative, String(stat.size), String(stat.mtimeNs), String(stat.ctimeNs), String(stat.ino), String(stat.mode)]);
    else throw Error('desktop_runtime_invalid');
  }
  visit(''); return entries;
}

export function preparePrimaryPayload(kit, cache) {
  return prepareArchive(kit.primaryArchive, kit.primaryArchiveSha256, 'primary', 'primary-runtime/runtime.json', cache);
}

export function preparePortableHost(kit, cache) {
  return prepareArchive(kit.hostArchive, kit.hostArchiveSha256, 'host', portableHostEntry(kit.platform), cache, kit.platform === 'darwin'
    ? root => execFileSync('/usr/bin/codesign', ['--verify', '--deep', '--strict', requireApp(root)], { timeout: 30000 }) : undefined);
}

const requireApp = root => path.join(root, 'Nexus Launcher.app');

function prepareArchive(archive, sha256, kind, entry, cache, verify) {
  fs.mkdirSync(cache, { recursive: true });
  const destination = path.join(cache, `${kind}-${sha256}`), stamp = `${destination}.json`;
  try {
    if (!fs.lstatSync(destination).isSymbolicLink() && JSON.stringify(runtimeInventory(destination)) === fs.readFileSync(stamp, 'utf8')) return destination;
  } catch {}
  const temporary = fs.mkdtempSync(path.join(cache, `.${kind}-${process.pid}-`));
  try {
    execFileSync(nativeTar(), ['-xf', archive, '-C', temporary], { windowsHide: true, timeout: 120000 });
    runtimeInventory(temporary); // Validate links before making the tree available.
    if (!fs.statSync(path.join(temporary, entry)).isFile()) throw Error('desktop_runtime_invalid');
    verify?.(temporary);
    // A junction is removed without traversing its target.
    if (fs.existsSync(destination)) fs.rmSync(destination, { recursive: true });
    fs.renameSync(temporary, destination);
    fs.writeFileSync(stamp, JSON.stringify(runtimeInventory(destination)));
    return destination;
  } finally { fs.rmSync(temporary, { recursive: true, force: true }); }
}
