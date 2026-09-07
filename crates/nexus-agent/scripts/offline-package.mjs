import { pipeline } from 'node:stream/promises';
// Embedded Nexus helper. Only filesystem packaging: no install/build/network.
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import crypto from 'node:crypto';
import { createRequire } from 'node:module';
import { spawnSync } from 'node:child_process';

const LIMIT = 12 * 1024 ** 3, COUNT = 250000, MANIFEST_LIMIT = 64 * 1024 ** 2;
const fail = message => { throw new Error(message); };
const slash = value => value.replaceAll('\\', '/');
const within = (root, value) => { const rel = path.relative(root, value); return rel === '' || (!path.isAbsolute(rel) && rel !== '..' && !rel.startsWith(`..${path.sep}`)); };
function safeName(name) {
  if (typeof name !== 'string' || !name || name.length > 4096 || name.includes('\\') || /[\x00-\x1f:]/.test(name) || name.startsWith('/')) fail('Unsafe offline package path');
  const parts = name.split('/');
  if (parts.some(p => !p || p === '.' || p === '..' || /[. ]$/.test(p) || /^(con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)/i.test(p))) fail('Unsupported Windows package path');
  if (!['slot', 'runtime', 'manifest.json'].includes(parts[0])) fail('Unknown offline package component');
  return name;
}
const excluded = name => { name = name.toLowerCase(); return ['.git', '.dsh', '.npmrc', '.pnpmfile.cjs'].includes(name) || name === '.env' || name.startsWith('.env.'); };
async function json(file, max = MANIFEST_LIMIT) {
  const stat = await fsp.lstat(file); if (!stat.isFile() || stat.isSymbolicLink() || stat.size > max) fail('Expected a bounded ordinary JSON file');
  return JSON.parse(await fsp.readFile(file, 'utf8'));
}
async function hash(file) {
  const h = crypto.createHash('sha256');
  for await (const bytes of fs.createReadStream(file)) h.update(bytes);
  return h.digest('hex');
}
function modules(runtime) {
  const require = createRequire(path.join(runtime, 'node/node_modules/npm/package.json'));
  return { tar: require('tar'), readShim: require('read-cmd-shim'), shim: require('cmd-shim') };
}
function version(runtime, relative) {
  const node = path.join(runtime, 'node/node.exe');
  const args = relative ? [path.join(runtime, relative), '--version'] : ['--version'];
  const result = spawnSync(node, args, { encoding: 'utf8', timeout: 20000, windowsHide: true,
    env: { ...process.env, PATH: `${path.dirname(node)}${path.delimiter}${process.env.SystemRoot || 'C:\\Windows'}\\System32`, NODE_OPTIONS: '', NODE_PATH: '', npm_config_offline: 'true' } });
  if (result.error || result.status !== 0 || !/^v?\d+\.\d+\.\d+(?:[-+].*)?\s*$/.test(result.stdout || '')) fail('Offline Node/npm/pnpm version probe failed');
  return result.stdout.trim();
}
async function runtimeIdentity(runtime) {
  for (const relative of ['node/node.exe', 'node/npm.cmd', 'node/node_modules/npm/bin/npm-cli.js', 'pnpm/bin/pnpm.cjs']) {
    if (!(await fsp.lstat(path.join(runtime, relative))).isFile()) fail('Offline runtime is incomplete');
  }
  return { node: version(runtime), npm: version(runtime, 'node/node_modules/npm/bin/npm-cli.js'), pnpm: version(runtime, 'pnpm/bin/pnpm.cjs') };
}
async function walkCopy(source, destination, component, links) {
  const sourceRoot = await fsp.realpath(source);
  let count = 0, total = 0;
  async function visit(relative) {
    const from = path.join(sourceRoot, relative), to = path.join(destination, relative), stat = await fsp.lstat(from);
    if (++count > COUNT) fail('Offline source has too many entries');
    if (stat.isSymbolicLink()) {
      const raw = await fsp.readlink(from), resolved = path.resolve(path.dirname(from), raw);
      if (!within(sourceRoot, resolved)) fail('Offline source link leaves its component');
      const real = await fsp.realpath(from); if (!within(sourceRoot, real)) fail('Offline source link resolves outside its component');
      const target = `${component}/${slash(path.relative(sourceRoot, resolved))}`;
      links.push({ path: safeName(`${component}/${slash(relative)}`), target: safeName(target), directory: (await fsp.stat(from)).isDirectory() });
      return;
    }
    if (stat.isDirectory()) {
      await fsp.mkdir(to, { recursive: true });
      for (const name of await fsp.readdir(from)) {
        if (excluded(name) || (component === 'slot' && !relative && name === 'manifest.json')) continue;
        await visit(path.join(relative, name));
      }
    } else if (stat.isFile()) {
      total += stat.size; if (total > LIMIT) fail('Offline source exceeds size budget');
      safeName(`${component}/${slash(relative)}`);
      await fsp.copyFile(from, to, fs.constants.COPYFILE_EXCL);
    } else fail('Offline source contains an unsupported special file');
  }
  await visit('');
}
function componentPath(name, slot, runtime) {
  safeName(name); const [component, ...rest] = name.split('/');
  if (!['slot', 'runtime'].includes(component)) fail('Invalid link component');
  return path.join(component === 'slot' ? slot : runtime, ...rest);
}
async function materialize(links, slot, runtime) {
  const names = new Set();
  const linkNames = new Set(links.map(link => link.path.toLowerCase()));
  for (const link of links) {
    const source = componentPath(link.path, slot, runtime), target = componentPath(link.target, slot, runtime);
    if (link.path.split('/')[0] !== link.target.split('/')[0] || names.has(link.path.toLowerCase())) fail('Invalid link graph');
    names.add(link.path.toLowerCase());
    for (let parent = path.posix.dirname(link.path); parent !== '.'; parent = path.posix.dirname(parent)) if (linkNames.has(parent.toLowerCase())) fail('A link cannot contain other entries');
    await fsp.mkdir(path.dirname(source), { recursive: true });
    try { await fsp.lstat(source); fail('Link destination already exists'); } catch (error) { if (error.code !== 'ENOENT') throw error; }
    await fsp.symlink(link.directory ? target : path.relative(path.dirname(source), target), source, link.directory ? 'junction' : 'file');
  }
  for (const link of links) {
    const source = componentPath(link.path, slot, runtime), root = link.path.startsWith('slot/') ? slot : runtime;
    if (!within(await fsp.realpath(root), await fsp.realpath(source))) fail('Materialized link escapes package');
  }
}
async function normalize(slot, tools) {
  async function visit(directory) {
    for (const entry of await fsp.readdir(directory, { withFileTypes: true })) {
      if (entry.isSymbolicLink()) continue;
      const file = path.join(directory, entry.name);
      if (entry.isDirectory()) await visit(file);
      else if (entry.name === '.modules.yaml' && path.basename(directory) === 'node_modules') {
        const value = await json(file, 16 * 1024 ** 2);
        if (!Number.isInteger(value.layoutVersion) || typeof value.virtualStoreDir !== 'string') fail('Unsupported pnpm modules metadata');
        value.virtualStoreDir = '.pnpm'; value.storeDir = '.nexus-offline-store';
        await fsp.writeFile(file, JSON.stringify(value, null, 2));
      } else if (entry.name.endsWith('.cmd') && path.basename(directory) === '.bin' && path.basename(path.dirname(directory)) === 'node_modules') {
        const destination = await tools.readShim(file);
        const target = path.resolve(directory, destination);
        if (!within(slot, target) || !within(await fsp.realpath(slot), await fsp.realpath(target))) fail('pnpm executable shim leaves the slot');
        await tools.shim(target, file.slice(0, -4));
      }
    }
  }
  await visit(slot);
}
async function inventory(root) {
  const entries = []; let total = 0;
  async function visit(relative) {
    const file = path.join(root, relative), stat = await fsp.lstat(file), name = safeName(slash(relative));
    if (entries.length >= COUNT) fail('Too many package files');
    if (stat.isDirectory()) {
      entries.push({ path: name, kind: 'directory' });
      for (const child of (await fsp.readdir(file)).sort()) await visit(path.join(relative, child));
    } else if (stat.isFile()) {
      total += stat.size; if (total > LIMIT) fail('Offline package is too large');
      entries.push({ path: name, kind: 'file', size: stat.size, sha256: await hash(file) });
    } else fail('Unexpected link or special file during package hashing');
  }
  await visit('slot'); await visit('runtime'); return { entries, total };
}
function validateManifest(manifest) {
  if (manifest.schema !== 1 || manifest.platform !== 'win32' || manifest.arch !== 'x64' || !Array.isArray(manifest.entries) || !Array.isArray(manifest.links)
    || manifest.entries.length + manifest.links.length > COUNT || typeof manifest.version !== 'string' || manifest.version.length > 200) fail('Unsupported offline manifest');
  const map = new Map(); let size = 0;
  for (const entry of manifest.entries) {
    const name = safeName(entry.path), lower = name.toLowerCase();
    if (map.has(lower) || !['file', 'directory'].includes(entry.kind)) fail('Duplicate or invalid manifest entry');
    if (entry.kind === 'file') { if (!Number.isSafeInteger(entry.size) || entry.size < 0 || !/^[a-f0-9]{64}$/.test(entry.sha256)) fail('Invalid file integrity metadata'); size += entry.size; }
    map.set(lower, entry);
  }
  if (size > LIMIT || size !== manifest.total) fail('Manifest exceeds size budget');
  for (const link of manifest.links) {
    safeName(link.path); safeName(link.target);
    if (typeof link.directory !== 'boolean' || map.has(link.path.toLowerCase()) || link.path.split('/')[0] !== link.target.split('/')[0]) fail('Invalid link metadata');
    map.set(link.path.toLowerCase(), { ...link, kind: 'link' });
  }
  for (const entry of map.values()) {
    let parent = path.posix.dirname(entry.path);
    while (parent !== '.') { if (map.get(parent.toLowerCase())?.kind !== 'directory') fail('Missing or non-directory archive parent'); parent = path.posix.dirname(parent); }
  }
  return map;
}
async function pack(job, tools) {
  const stage = path.join(job.work, 'payload'); await fsp.mkdir(stage);
  const slot = path.join(stage, 'slot'), runtime = path.join(stage, 'runtime'), links = [];
  process.stderr.write('Offline: copying slot and runtime\n');
  await walkCopy(job.slot, slot, 'slot', links); await walkCopy(job.runtime, runtime, 'runtime', links);
  process.stderr.write(`Offline: normalizing ${links.length} internal links and executable shims\n`);
  await materialize(links, slot, runtime); await normalize(slot, tools);
  const versions = await runtimeIdentity(runtime);
  for (const link of links.reverse()) await fsp.unlink(componentPath(link.path, slot, runtime)); links.reverse();
  process.stderr.write('Offline: hashing normalized files\n');
  const files = await inventory(stage);
  const manifest = { schema: 1, platform: 'win32', arch: 'x64', nexus: job.nexus, version: job.version, versions, links, ...files };
  validateManifest(manifest);
  const text = JSON.stringify(manifest); if (Buffer.byteLength(text) > MANIFEST_LIMIT) fail('Manifest is too large');
  await fsp.writeFile(path.join(stage, 'manifest.json'), text);
  const temp = `${job.archive}.nexus-${job.id}.tmp`;
  const writable = await fsp.open(temp, 'wx');
  try {
    try { await fsp.lstat(job.archive); fail('Export destination already exists'); } catch (error) { if (error.code !== 'ENOENT') throw error; }
    process.stderr.write(`Offline: writing archive (${files.entries.length} entries)\n`);
    await pipeline(tools.tar.c({ cwd: stage, gzip: true, portable: true, noMtime: true, strict: true }, ['manifest.json', 'slot', 'runtime']), writable.createWriteStream());
    const file = await fsp.open(temp, 'r+'); await file.sync(); await file.close();
    await fsp.link(temp, job.archive);
  } finally { await writable.close().catch(() => {}); await fsp.unlink(temp).catch(() => {}); }
  await fsp.writeFile(path.join(job.work, 'result.json'), JSON.stringify({ version: manifest.version, versions, archive: job.archive }));
}
async function unpack(job, tools) {
  const stage = path.join(job.work, 'payload'); await fsp.mkdir(stage);
  const manifestChunks = []; let bytes = 0, count = 0; const seen = new Set();
  await tools.tar.t({ file: job.archive, strict: true, onentry(entry) {
    const name = safeName(entry.path.replace(/\/$/, ''));
    if (!['File', 'Directory'].includes(entry.type) || seen.has(name.toLowerCase()) || ++count > COUNT) fail('Unsupported or duplicate tar entry');
    seen.add(name.toLowerCase()); bytes += entry.size; if (bytes > LIMIT + MANIFEST_LIMIT) fail('Tar size budget exceeded');
    if (name === 'manifest.json') { if (entry.type !== 'File' || entry.size > MANIFEST_LIMIT) fail('Invalid tar manifest'); entry.on('data', chunk => { manifestChunks.push(chunk); }); }
  } });
  const manifest = JSON.parse(Buffer.concat(manifestChunks).toString('utf8')), expected = validateManifest(manifest); const extracted = new Set();
  await tools.tar.x({ file: job.archive, cwd: stage, strict: true, preservePaths: false, noChmod: true, filter(name, entry) {
    name = safeName(name.replace(/\/$/, '')); const lower = name.toLowerCase();
    if (extracted.has(lower) || !['File', 'Directory'].includes(entry.type)) fail('Tar changed during extraction'); extracted.add(lower);
    if (name === 'manifest.json') return entry.type === 'File' && entry.size <= MANIFEST_LIMIT;
    const item = expected.get(lower);
    if (!item || item.path !== name || (item.kind === 'file' ? entry.type !== 'File' || entry.size !== item.size : entry.type !== 'Directory')) fail('Tar entry differs from manifest');
    return true;
  } });
  if (JSON.stringify(await json(path.join(stage, 'manifest.json'))) !== JSON.stringify(manifest)) fail('Tar manifest changed during extraction');
  const actual = await inventory(stage);
  if (JSON.stringify(actual.entries) !== JSON.stringify(manifest.entries) || actual.total !== manifest.total) fail('Offline file integrity check failed');
  for (const required of ['slot/apps/cli/lib/bin.js', 'runtime/node/node.exe', 'runtime/node/npm.cmd', 'runtime/node/node_modules/npm/bin/npm-cli.js', 'runtime/pnpm/bin/pnpm.cjs']) if (expected.get(required)?.kind !== 'file') fail('Offline package is incomplete');
  await fsp.writeFile(path.join(job.work, 'result.json'), JSON.stringify({ version: manifest.version, versions: manifest.versions, archive: job.archive }));
}
async function main() {
  if (process.platform !== 'win32' || process.arch !== 'x64') fail('Offline packages currently support Windows x64');
  const job = await json(process.argv[2], 65536); const tools = modules(job.tools);
  if (!path.isAbsolute(job.work) || !path.isAbsolute(job.archive)) fail('Offline paths must be absolute');
  if (job.action === 'export') await pack(job, tools);
  else if (job.action === 'import') await unpack(job, tools);
  else if (job.action === 'finalize') {
    const manifest = await json(path.join(job.work, 'payload/manifest.json')); validateManifest(manifest);
    await materialize(manifest.links, job.slot, job.runtime);
    const observed = await runtimeIdentity(job.runtime);
    if (JSON.stringify(observed) !== JSON.stringify(manifest.versions)) fail('Offline runtime versions differ from manifest');
  } else fail('Unknown offline operation');
}
main().catch(error => { process.stderr.write(`Offline package: ${error.message}\n`); process.exitCode = 1; });
