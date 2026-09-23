import fs from 'node:fs';
import path from 'node:path';
import { portableHostEntry } from './desktop-runtime.mjs';
import { execFileSync } from 'node:child_process';

export const nativeTar = () => process.platform === 'win32'
  ? path.join(process.env.SystemRoot || 'C:\\Windows', 'System32/tar.exe') : '/usr/bin/tar';

// Include every entry; link targets cannot escape the owned tree. Metadata
// changes invalidate the cache, including same-size edits with restored mtime.
export async function runtimeInventory(directory) {
  const root = await fs.promises.realpath(directory);
  let active = 0; const queue = [];
  // Bound filesystem requests, not whole recursive visits: parents must not
  // retain a slot while waiting for children. Every entry is still checked.
  async function io(run) {
    if (active >= 16) await new Promise(resolve => queue.push(resolve));
    else active++;
    try { return await run(); }
    finally { if (queue.length) queue.shift()(); else active--; }
  }
  async function visit(relative) {
    const file = path.join(root, relative), stat = await io(() => fs.promises.lstat(file, { bigint: true }));
    if (stat.isSymbolicLink()) {
      const target = await io(() => fs.promises.realpath(file)), within = path.relative(root, target);
      if (within === '..' || within.startsWith(`..${path.sep}`) || path.isAbsolute(within)) throw Error('desktop_runtime_invalid');
      return [[relative, 'link', await io(() => fs.promises.readlink(file)), target]];
    } else if (stat.isDirectory()) {
      const names = (await io(() => fs.promises.readdir(file))).sort();
      // Drain sibling reads before rejection so cache cleanup cannot race them.
      const children = await Promise.allSettled(names.map(name => visit(path.join(relative, name))));
      const failed = children.find(child => child.status === 'rejected');
      if (failed) throw failed.reason;
      return [[relative, 'directory'], ...children.flatMap(child => child.value)];
    } else if (stat.isFile()) return [[relative, String(stat.size), String(stat.mtimeNs), String(stat.ctimeNs), String(stat.ino), String(stat.mode)]];
    else throw Error('desktop_runtime_invalid');
  }
  return visit('');
}

export function preparePrimaryPayload(kit, cache) {
  return prepareArchive(kit.primaryArchive, kit.primaryArchiveSha256, 'primary', 'primary-runtime/runtime.json', cache);
}

export function preparePortableHost(kit, cache) {
  return prepareArchive(kit.hostArchive, kit.hostArchiveSha256, 'host', portableHostEntry(kit.platform), cache, kit.platform === 'darwin'
    ? root => verifyMacApp(requireApp(root)) : undefined);
}

export function verifyMacApp(app, run = execFileSync) {
  try {
    run('/usr/bin/codesign', ['--verify', '--deep', '--strict', app], { timeout: 120000, encoding: 'utf8' });
  } catch (error) {
    const reason = error.code === 'ETIMEDOUT'
      ? 'macOS app signature verification timed out after 120 seconds'
      : `macOS app signature verification failed: ${String(error.stderr || error.message).trim()}`;
    throw new Error(reason, { cause: error });
  }
}

const requireApp = root => path.join(root, 'Nexus Launcher.app');

async function prepareArchive(archive, sha256, kind, entry, cache, verify) {
  fs.mkdirSync(cache, { recursive: true });
  const destination = path.join(cache, `${kind}-${sha256}`), stamp = `${destination}.json`;
  try {
    if (!fs.lstatSync(destination).isSymbolicLink() && JSON.stringify(await runtimeInventory(destination)) === fs.readFileSync(stamp, 'utf8')) return destination;
  } catch {}
  const temporary = fs.mkdtempSync(path.join(cache, `.${kind}-${process.pid}-`));
  try {
    execFileSync(nativeTar(), ['-xf', archive, '-C', temporary], { windowsHide: true, timeout: 120000 });
    const files = await runtimeInventory(temporary); // Validate links before publication.
    if (!fs.statSync(path.join(temporary, entry)).isFile()) throw Error('desktop_runtime_invalid');
    verify?.(temporary);
    // A junction is removed without traversing its target.
    if (fs.existsSync(destination)) fs.rmSync(destination, { recursive: true });
    fs.renameSync(temporary, destination);
    // Renaming the owned tree preserves file identity and timestamps. Recheck
    // links at their final location; avoid restatting every extracted file.
    for (const entry of files) if (entry[1] === 'link') {
      const resolved = fs.realpathSync(path.join(destination, entry[0]));
      const relative = path.relative(destination, resolved);
      if (relative === '..' || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) throw Error('desktop_runtime_invalid');
      entry[3] = resolved;
    }
    fs.writeFileSync(stamp, JSON.stringify(files));
    return destination;
  } finally { fs.rmSync(temporary, { recursive: true, force: true }); }
}
