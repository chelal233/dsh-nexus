import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const inside = (root, file) => {
  const relative = path.relative(root, file);
  return relative !== '' && relative !== '..' && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative);
};

// Package managers own the profile, not the source installation. Never give
// their directory replacement code a projection into the running workspace.
export function detachDesktopSourceLinks({ profile, source }) {
  const root = fs.realpathSync.native(source);
  const base = fs.realpathSync.native(profile);
  const pending = [];
  const inspect = dir => {
    let stat;
    try { stat = fs.lstatSync(dir); } catch (error) { if (error.code === 'ENOENT') return; throw error; }
    if (!stat.isDirectory() || stat.isSymbolicLink() || !inside(base, fs.realpathSync.native(dir))) {
      throw new Error('Desktop package directory is redirected; refusing package installation');
    }
    for (const item of fs.readdirSync(dir, { withFileTypes: true })) {
      const entry = path.join(dir, item.name);
      if (item.name.startsWith('@')) { inspect(entry); continue; }
      if (!fs.lstatSync(entry).isSymbolicLink()) continue;
      let target;
      try { target = fs.realpathSync.native(entry); }
      catch (error) { if (error.code === 'ENOENT') continue; throw error; }
      if (target === root || inside(root, target)) pending.push({ entry, link: fs.readlinkSync(entry) });
    }
  };
  // Inspect both trees before unlinking anything. Removing the private fallback
  // as well prevents it from projecting the same source link back into pnpm.
  inspect(path.join(base, 'node_modules'));
  inspect(path.join(base, '.dsh-module-fallback', 'node_modules'));
  for (const { entry, link } of pending) {
    // Another writer must never turn this into a recursive deletion of files.
    if (!fs.lstatSync(entry).isSymbolicLink() || fs.readlinkSync(entry) !== link) {
      throw new Error('Desktop package link changed during installation preparation');
    }
    fs.unlinkSync(entry);
  }
  return pending.length;
}

export async function runDesktopPnpm(options) {
  const args = process.argv.slice(2);
  const readOnly = ['view', 'info', '--version', '-v', '--help', '-h'].includes(args[0]);
  if (!readOnly) {
    if (fs.realpathSync.native(process.cwd()) !== fs.realpathSync.native(options.profile)) {
      throw new Error('Desktop package installation must run inside its configured profile');
    }
    const count = detachDesktopSourceLinks(options);
    if (count) console.log(`[Nexus] Detached ${count} source links before package installation`);
  }
  process.argv[1] = options.pnpm;
  await import(pathToFileURL(options.pnpm).href);
}

export function prepareDesktopPnpm({ source, home, userData }) {
  const options = {
    source, profile: path.join(home, 'profiles', 'desktop'),
    pnpm: path.join(source, 'apps/desktop/node_modules/pnpm/bin/pnpm.mjs'),
  };
  const modulePath = fileURLToPath(import.meta.url).replace(/app\.asar([\\/])/, 'app.asar.unpacked$1');
  const entry = path.join(userData, 'nexus-pnpm.mjs');
  fs.mkdirSync(userData, { recursive: true });
  fs.writeFileSync(entry, `import { runDesktopPnpm } from ${JSON.stringify(pathToFileURL(modulePath).href)};\nawait runDesktopPnpm(${JSON.stringify(options)});\n`, { mode: 0o600 });
  return entry;
}
