import { pipeline } from 'node:stream/promises';
import { Transform } from 'node:stream';
// Embedded Nexus helper. Only filesystem packaging: no install/build/network.
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import crypto from 'node:crypto';
import { createRequire } from 'node:module';
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { isDeepStrictEqual } from 'node:util';

const LIMIT = 12 * 1024 ** 3, COUNT = 250000, MANIFEST_LIMIT = 64 * 1024 ** 2;
const windows = process.platform === 'win32';
const nodeEntry = windows ? 'node/node.exe' : 'node/node';
const npmEntry = windows ? 'node/npm.cmd' : 'node/npm';
const fail = message => { throw new Error(message); };
const slash = value => value.replaceAll('\\', '/');
let progressFile, progressId, progressState, lastProgress = 0;
export function writeProgress(file, state, io = fs) {
  try {
    io.writeFileSync(`${file}.next`, JSON.stringify(state));
    io.renameSync(`${file}.next`, file);
    return true;
  } catch {
    // Windows readers can briefly prevent atomic replacement. Progress is
    // advisory: retain the last complete snapshot and retry on the next tick.
    return false;
  }
}
function report(stage, completed = 0, total = null, unit = 'files') {
  const now = Date.now(), changed = progressState?.stage !== stage;
  progressState = { operation_id: progressId, stage, completed, total, unit,
    stage_started_at: changed ? now : progressState.stage_started_at, updated_at: now };
  if (!progressFile || (!changed && now - lastProgress < 500 && completed !== total)) return;
  lastProgress = now;
  writeProgress(progressFile, progressState);
}
async function parallel(items, action) {
  let index = 0;
  await Promise.all(Array.from({ length: Math.min(4, items.length) }, async () => {
    while (index < items.length) { const item = items[index++]; await action(item); }
  }));
}
const within = (root, value) => { const rel = path.relative(root, value); return rel === '' || (!path.isAbsolute(rel) && rel !== '..' && !rel.startsWith(`..${path.sep}`)); };
function safeName(name) {
  if (typeof name !== 'string' || !name || name.length > 4096 || name.includes('\\') || /[\x00-\x1f:]/.test(name) || name.startsWith('/')) fail('Unsafe offline package path');
  const parts = name.split('/');
  if (parts.some(p => !p || p === '.' || p === '..' || /[. ]$/.test(p) || /^(con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)/i.test(p))) fail('Unsupported Windows package path');
  if (!['slot', 'runtime', 'environment', 'manifest.json'].includes(parts[0])) fail('Unknown offline package component');
  return name;
}
// Generated Desktop views contain machine-specific links. The runtime component
// carries the offline assets needed to recreate them without network access.
const excluded = name => { name = name.toLowerCase(); return ['.git', '.dsh', '.desktop-build', '.npmrc', '.pnpmfile.cjs'].includes(name) || name === '.env' || name.startsWith('.env.'); };
async function json(file, max = MANIFEST_LIMIT) {
  const stat = await fsp.lstat(file); if (!stat.isFile() || stat.isSymbolicLink() || stat.size > max) fail('Expected a bounded ordinary JSON file');
  return JSON.parse(await fsp.readFile(file, 'utf8'));
}
async function hash(file) {
  const h = crypto.createHash('sha256');
  for await (const bytes of fs.createReadStream(file)) h.update(bytes);
  return h.digest('hex');
}
export function modules(runtime) {
  // Rust canonical paths use the Windows namespace prefix. Node's nested CJS
  // loader can misinterpret it as a drive-relative path; normalize only the
  // module-loading anchor, preserving canonical paths for filesystem checks.
  const require = createRequire(path.join(fs.realpathSync.native(runtime), 'node/node_modules/npm/package.json'));
  return { tar: require('tar'), readShim: require('read-cmd-shim'), shim: require('cmd-shim') };
}
function version(runtime, relative) {
  const node = path.join(runtime, nodeEntry);
  const args = relative ? [path.join(runtime, relative), '--version'] : ['--version'];
  const result = spawnSync(node, args, { encoding: 'utf8', timeout: 20000, windowsHide: true,
    env: { ...process.env, PATH: `${path.dirname(node)}${path.delimiter}${windows ? `${process.env.SystemRoot || 'C:\\Windows'}\\System32` : '/usr/bin:/bin'}`, NODE_OPTIONS: '', NODE_PATH: '', npm_config_offline: 'true' } });
  if (result.error || result.status !== 0 || !/^v?\d+\.\d+\.\d+(?:[-+].*)?\s*$/.test(result.stdout || '')) fail('Offline Node/npm/pnpm version probe failed');
  return result.stdout.trim();
}
async function runtimeIdentity(runtime) {
  for (const relative of [nodeEntry, npmEntry, 'node/node_modules/npm/bin/npm-cli.js', 'pnpm/bin/pnpm.cjs']) {
    if (!(await fsp.lstat(path.join(runtime, relative))).isFile()) fail('Offline runtime is incomplete');
  }
  return { node: version(runtime), npm: version(runtime, 'node/node_modules/npm/bin/npm-cli.js'), pnpm: version(runtime, 'pnpm/bin/pnpm.cjs') };
}
async function walkCopy(source, destination, component, links) {
  report(`measure_${component}`);
  const sourceRoot = await fsp.realpath(source);
  let count = 0, total = 0; const files = [];
  async function visit(relative) {
    const from = path.join(sourceRoot, relative), to = path.join(destination, relative), stat = await fsp.lstat(from);
    if (++count > COUNT) fail('Offline source has too many entries');
    report(`measure_${component}`, count);
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
        if (excluded(name) || (!relative && ((component === 'slot' && name === 'manifest.json') || (component === 'runtime' && name === 'environment')))) continue;
        await visit(path.join(relative, name));
      }
    } else if (stat.isFile()) {
      total += stat.size; if (total > LIMIT) fail('Offline source exceeds size budget');
      safeName(`${component}/${slash(relative)}`);
      files.push({ from, to });
    } else fail('Offline source contains an unsupported special file');
  }
  await visit('');
  let copied = 0;
  report(`copy_${component}`, 0, files.length);
  await parallel(files, async ({ from, to }) => { await fsp.copyFile(from, to, fs.constants.COPYFILE_EXCL); report(`copy_${component}`, ++copied, files.length); });
}
function componentPath(name, slot, runtime, environment = path.join(path.dirname(slot), 'environment')) {
  safeName(name); const [component, ...rest] = name.split('/');
  if (!['slot', 'runtime', 'environment'].includes(component)) fail('Invalid link component');
  return path.join(component === 'slot' ? slot : component === 'runtime' ? runtime : environment, ...rest);
}
const allowedLink = (source, target) => source.split('/')[0] === target.split('/')[0] || (source.startsWith('environment/') && target.startsWith('slot/'));
async function materialize(links, slot, runtime, environment) {
  let done = 0; report('restore_links', 0, links.length);
  const names = new Set();
  const linkNames = new Set(links.map(link => link.path.toLowerCase()));
  for (const link of links) {
    const source = componentPath(link.path, slot, runtime, environment), target = componentPath(link.target, slot, runtime, environment);
    if (!allowedLink(link.path, link.target) || names.has(link.path.toLowerCase())) fail('Invalid link graph');
    names.add(link.path.toLowerCase());
    for (let parent = path.posix.dirname(link.path); parent !== '.'; parent = path.posix.dirname(parent)) if (linkNames.has(parent.toLowerCase())) fail('A link cannot contain other entries');
    await fsp.mkdir(path.dirname(source), { recursive: true });
    try { await fsp.lstat(source); fail('Link destination already exists'); } catch (error) { if (error.code !== 'ENOENT') throw error; }
    await fsp.symlink(windows && link.directory ? target : path.relative(path.dirname(source), target), source, link.directory ? (windows ? 'junction' : 'dir') : 'file');
    report('restore_links', ++done, links.length);
  }
  for (const link of links) {
    const source = componentPath(link.path, slot, runtime, environment), root = componentPath(link.target.split('/')[0], slot, runtime, environment);
    if (!within(await fsp.realpath(root), await fsp.realpath(source))) fail('Materialized link escapes package');
  }
}
async function normalize(slot, tools) {
  let visited = 0; report('normalize');
  async function visit(directory) {
    for (const entry of await fsp.readdir(directory, { withFileTypes: true })) {
      if (entry.isSymbolicLink()) continue;
      const file = path.join(directory, entry.name);
      report('normalize', ++visited);
      if (entry.isDirectory()) await visit(file);
      else if (entry.name === '.modules.yaml' && path.basename(directory) === 'node_modules') {
        const value = await json(file, 16 * 1024 ** 2);
        if (!Number.isInteger(value.layoutVersion) || typeof value.virtualStoreDir !== 'string') fail('Unsupported pnpm modules metadata');
        value.virtualStoreDir = '.pnpm'; value.storeDir = '.nexus-offline-store';
        await fsp.writeFile(file, JSON.stringify(value, null, 2));
      } else if ((windows ? entry.name.endsWith('.cmd') : !/\.(cmd|ps1)$/.test(entry.name)) && path.basename(directory) === '.bin' && path.basename(path.dirname(directory)) === 'node_modules') {
        const destination = await tools.readShim(file);
        const target = path.resolve(directory, destination);
        if (!within(slot, target) || !within(await fsp.realpath(slot), await fsp.realpath(target))) fail('pnpm executable shim leaves the slot');
        await tools.shim(target, windows ? file.slice(0, -4) : file);
      }
    }
  }
  await visit(slot);
}
async function inventory(root, environment = false, runtime = true) {
  const entries = []; let total = 0;
  report('scan_files');
  async function visit(relative) {
    const file = path.join(root, relative), stat = await fsp.lstat(file), name = safeName(slash(relative));
    if (entries.length >= COUNT) fail('Too many package files');
    report('scan_files', entries.length);
    if (stat.isDirectory()) {
      entries.push({ path: name, kind: 'directory' });
      for (const child of (await fsp.readdir(file)).sort()) await visit(path.join(relative, child));
    } else if (stat.isFile()) {
      total += stat.size; if (total > LIMIT) fail('Offline package is too large');
      entries.push({ path: name, kind: 'file', size: stat.size, ...(!windows ? { mode: stat.mode & 0o777 } : {}) });
    } else fail('Unexpected link or special file during package hashing');
  }
  if (runtime) { await visit('slot'); await visit('runtime'); }
  if (environment) await visit('environment');
  const files = entries.filter(entry => entry.kind === 'file'); let done = 0;
  report('hash_files', 0, files.length);
  await parallel(files, async entry => { entry.sha256 = await hash(path.join(root, entry.path)); report('hash_files', ++done, files.length); });
  return { entries, total };
}
function validateManifest(manifest) {
  if (![1, 2, 3].includes(manifest.schema) || manifest.platform !== process.platform || manifest.arch !== process.arch || !Array.isArray(manifest.entries) || !Array.isArray(manifest.links)
    || manifest.entries.length + manifest.links.length > COUNT || typeof manifest.version !== 'string' || manifest.version.length > 200) fail('Unsupported offline manifest');
  const map = new Map(); let size = 0;
  for (const entry of manifest.entries) {
    const name = safeName(entry.path), lower = name.toLowerCase();
    if (map.has(lower) || !['file', 'directory'].includes(entry.kind)) fail('Duplicate or invalid manifest entry');
    if (entry.kind === 'file') { if (!Number.isSafeInteger(entry.size) || entry.size < 0 || !/^[a-f0-9]{64}$/.test(entry.sha256)) fail('Invalid file integrity metadata'); size += entry.size; }
    map.set(lower, entry);
  }
  if (!windows && [...map.values()].some(entry => entry.kind === 'file' && (!Number.isInteger(entry.mode) || entry.mode < 0 || entry.mode > 0o777))) fail('Invalid executable mode metadata');
  if (size > LIMIT || size !== manifest.total) fail('Manifest exceeds size budget');
  for (const link of manifest.links) {
    safeName(link.path); safeName(link.target);
    if (typeof link.directory !== 'boolean' || map.has(link.path.toLowerCase()) || !allowedLink(link.path, link.target)) fail('Invalid link metadata');
    map.set(link.path.toLowerCase(), { ...link, kind: 'link' });
  }
  for (const entry of map.values()) {
    let parent = path.posix.dirname(entry.path);
    while (parent !== '.') { if (map.get(parent.toLowerCase())?.kind !== 'directory') fail('Missing or non-directory archive parent'); parent = path.posix.dirname(parent); }
  }
  if (manifest.schema === 1 && [...map.keys()].some(name => name.startsWith('environment'))) fail('Unexpected legacy environment component');
  if (manifest.schema >= 2) {
    const contents = manifest.contents;
    if (!contents || !Array.isArray(contents.profiles) || contents.profiles.length > 32
      || new Set(contents.profiles.map(name => String(name).toLowerCase())).size !== contents.profiles.length
      || !contents.profiles.every(validProfile) || !['configuration', 'plugins', 'credentials'].every(key => typeof contents[key] === 'boolean')
      || (contents.profiles.length && !contents.profiles.includes(manifest.active_profile))) fail('Invalid environment contents');
    if (manifest.schema === 3 && (!['runtime', 'environment', 'sessions'].every(key => typeof contents[key] === 'boolean')
      || (!contents.runtime && !contents.profiles.length && !contents.environment && !contents.sessions && !contents.credentials)
      || (!contents.runtime && [...map.keys()].some(name => /^(slot|runtime)(\/|$)/.test(name))))) fail('Invalid selected package components');
    for (const name of contents.profiles) if (map.get(`environment/profiles/${name}/package.json`.toLowerCase())?.kind !== 'file') fail('Missing imported profile manifest');
  }
  return map;
}
async function volumeSpace(target) {
  const real = await fsp.realpath(target), stat = await fsp.stat(real), space = await fsp.statfs(real, { bigint: true });
  return { key: String(stat.dev), available: space.bavail * space.bsize, blockSize: space.bsize };
}
// Additional simultaneous allocations only: neither existing source files nor
// the final archive's hard link are a second copy. bavail respects user quota.
export async function ensureSpaceBudget(entries, probe = volumeSpace) {
  const volumes = new Map();
  for (const entry of entries) {
    const space = await probe(entry.path), required = BigInt(entry.bytes) + BigInt(entry.entries || 0) * space.blockSize;
    const saved = volumes.get(space.key) || { required: 0n, available: space.available };
    saved.required += required; if (space.available < saved.available) saved.available = space.available;
    volumes.set(space.key, saved);
  }
  for (const { required, available } of volumes.values()) if (required > available) fail(`Insufficient target-volume space: ${available} bytes available to this user, ${required} additional bytes required`);
}
async function measureSource(root, component) {
  report(`measure_${component}`);
  let bytes = 0, entries = 0;
  async function visit(relative) {
    const file = path.join(root, relative), stat = await fsp.lstat(file);
    if (++entries > COUNT) fail('Offline source has too many entries');
    report(`measure_${component}`, entries);
    if (stat.isSymbolicLink()) return;
    if (stat.isDirectory()) {
      for (const name of await fsp.readdir(file)) if (!excluded(name) && !(!relative && ((component === 'slot' && name === 'manifest.json') || (component === 'runtime' && name === 'environment')))) await visit(path.join(relative, name));
    } else if (stat.isFile()) { bytes += stat.size; if (bytes > LIMIT) fail('Offline source is too large'); }
    else fail('Unsupported offline source file');
  }
  await visit(''); return { bytes, entries };
}
const archiveBudget = (bytes, entries) => Math.ceil((bytes + (entries + 2) * 8192) * 1.01) + 1024 ** 2;
const validProfile = name => typeof name === 'string' && /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$/.test(name) && !name.endsWith('.') && !['node_modules', 'con', 'prn', 'aux', 'nul'].includes(name.toLowerCase());
const normalizedKey = key => key.replace(/([a-z\d])([A-Z])/g, '$1_$2').replaceAll('-', '_').toLowerCase();
// Exact setting names and types distinguish controls from credential values.
// Unknown credential-looking fields remain conservative; a string containing
// a secret is never exempted merely because its name contains "limit".
const ordinarySecretSetting = (key, value) => {
  const name = normalizedKey(key);
  return typeof value === 'boolean' && ['password_enabled', 'token_enabled', 'authorization_enabled', 'cookies_enabled'].includes(name)
    || typeof value === 'number' && Number.isFinite(value) && ['token_limit', 'token_count', 'token_budget', 'token_timeout', 'token_expiry', 'password_min_length', 'password_max_length'].includes(name);
};
const secretKey = (key, value) => !ordinarySecretSetting(key, value) && /(?:^|_)(?:api_keys?|password|passwd|secrets?|token|authorization|credentials?|cookies?|private_key)(?:$|_)/i.test(normalizedKey(key));
const pathKey = key => /(?:^|_)(?:paths?|files?|director(?:y|ies)|dirs?|roots?|homes?)$/.test(normalizedKey(key));
const normalizedPath = value => slash(value).replace(/^\/\/\?\/UNC\//i, '//').replace(/^\/\/\?\//, '').replace(/\/$/, '');
function relocatePath(value, from, to) {
  const text = normalizedPath(value), root = normalizedPath(from);
  const windows = /^[a-z]:\//i.test(root) || root.startsWith('//');
  const left = windows ? text.toLowerCase() : text, right = windows ? root.toLowerCase() : root;
  return left === right || left.startsWith(right + '/') ? to + text.slice(root.length) : value;
}
export function relocateConfiguration(value, from, to, key = '') {
  if (Array.isArray(value)) return value.map(item => relocateConfiguration(item, from, to, key));
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([name,item]) => [name,relocateConfiguration(item,from,to,name)]));
  // Only named path fields are rewritten, including credential file paths.
  // Opaque credentials, URLs, prose and conversation data keep their bytes.
  return typeof value === 'string' && pathKey(key) ? relocatePath(value, from, normalizedPath(to)) : value;
}
export function portableConfiguration(value, credentials, roots = {}, field = '') {
  if (Array.isArray(value)) return value.map(item => portableConfiguration(item, credentials, roots, field));
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value)
    .filter(([key,item]) => credentials || !secretKey(key,item)).map(([key, item]) => [key, portableConfiguration(item, credentials, roots, key)]));
  if (typeof value !== 'string') return value;
  let text = value;
  if (!credentials) {
    text = text.replace(/(https?:\/\/)[^\s/@]+:[^\s/@]+@/gi, '$1');
    text = text.replace(/([?&](?:api[_-]?key|token|secret|password)=)[^&#\s]+/gi, '$1');
  }
  for (const [name, root] of Object.entries(roots)) {
    if (!root) continue;
    if (!secretKey(field, value) || pathKey(field)) text = relocatePath(text, root, `__NEXUS_OFFLINE_${name}__`);
  }
  return text;
}

export function keepCredentials(next, previous, onPreserved = () => {}) {
  const keep = (oldValue, newValue) => {
    if (!isDeepStrictEqual(oldValue, newValue)) onPreserved();
    return oldValue;
  };
  const sensitive = value => JSON.stringify(portableConfiguration(value, false)) !== JSON.stringify(value);
  const conflict = () => fail('Existing credentials belong to configuration absent from the package. Exclude that configuration or explicitly choose Replace with package credentials; the current environment was preserved');
  if (Array.isArray(next) && Array.isArray(previous)) {
    const matched = new Set();
    const result = next.map(item => {
      const identity = item && typeof item === 'object' && ['id', 'name'].find(key => typeof item[key] === 'string');
      const old = identity ? previous.find(value => value?.[identity] === item[identity]) : previous.find(value => JSON.stringify(portableConfiguration(value, false)) === JSON.stringify(portableConfiguration(item, false)));
      if (old === undefined) return item;
      matched.add(old); return keepCredentials(item, old, onPreserved);
    });
    if (previous.some(value => !matched.has(value) && sensitive(value))) conflict();
    return result;
  }
  if (next && previous && typeof next === 'object' && typeof previous === 'object' && !Array.isArray(next) && !Array.isArray(previous)) {
    // A type change must not turn a protected credential into an ordinary
    // control (or vice versa). Both sides must qualify before accepting it.
    const result = Object.fromEntries(Object.entries(next).map(([key, value]) => [key, Object.hasOwn(previous, key) ? secretKey(key,previous[key]) || secretKey(key,value) ? keep(previous[key], value) : keepCredentials(value, previous[key], onPreserved) : value]));
    for (const [key, value] of Object.entries(previous)) {
      if (secretKey(key,value)) {
        Object.defineProperty(result, key, { value, enumerable: true, configurable: true, writable: true });
      }
      else if (!Object.hasOwn(next, key) && sensitive(value)) conflict();
    }
    return result;
  }
  if (previous && typeof previous === 'object' && sensitive(previous)) conflict();
  return typeof previous === 'string' && portableConfiguration(previous, false) !== previous ? keep(previous,next) : next;
}
function configurationValue(text, job) {
  try { return JSON.parse(text); } catch {
    if (!job.private_writer) fail('Nexus configuration parser is unavailable');
    const parsed = spawnSync(job.private_writer, ['--parse-offline-yaml'], { input:text, encoding:'utf8', windowsHide:true, timeout:30000, maxBuffer:4*1024**2 });
    // Never include parser output in an error: it may contain credentials.
    if (parsed.error || parsed.status !== 0) fail('Configuration contains unsupported YAML syntax or tags');
    return JSON.parse(parsed.stdout);
  }
}
async function captureEnvironment(job, stage, links) {
  const contents = job.contents;
  if (!contents || (!contents.profiles.length && !contents.environment && !contents.sessions && !contents.credentials)) return null;
  if (contents.profiles.length > 32 || !contents.profiles.every(validProfile)) fail('Invalid selected profiles');
  const environment = path.join(stage, 'environment'); await fsp.mkdir(path.join(environment, 'profiles'), { recursive: true });
  const sourceSlot = job.slot ? await fsp.realpath(job.slot) : null, home = await fsp.realpath(job.home);
  const roots = [], queue = [], configFiles = [], files = []; let count = 0, bytes = 0;
  const mapped = real => {
    if (contents.runtime && sourceSlot && within(sourceSlot, real)) return `slot/${slash(path.relative(sourceSlot, real))}`;
    const root = roots.filter(item => within(item.source, real)).sort((a, b) => b.source.length - a.source.length)[0];
    return root ? `${root.target}${real === root.source ? '' : '/' + slash(path.relative(root.source, real))}` : null;
  };
  function addUnit(real) {
    const parts = real.split(path.sep), index = parts.lastIndexOf('.pnpm');
    const source = index >= 0 && parts.length > index + 2 ? parts.slice(0, index + 2).join(path.sep) : real;
    const target = `environment/.packages/${crypto.createHash('sha256').update(source.toLowerCase()).digest('hex').slice(0, 24)}`;
    roots.push({ source, target }); queue.push({ source, target }); return mapped(real);
  }
  async function copyTree(source, target) {
    if (++count > COUNT) fail('Selected plugins exceed package entry limit');
    report('copy_environment', count);
    const stat = await fsp.lstat(source), destination = path.join(stage, target);
    if (stat.isSymbolicLink()) {
      const real = await fsp.realpath(source), targetStat = await fsp.stat(real);
      let linkTarget = mapped(real);
      if (!linkTarget) {
        if (!targetStat.isDirectory() || !(await fsp.stat(path.join(real, 'package.json')).catch(() => null))?.isFile()) fail('Plugin link is not a self-contained installed package');
        linkTarget = addUnit(real);
      }
      links.push({ path: safeName(target), target: safeName(linkTarget), directory: targetStat.isDirectory() }); return;
    }
    if (stat.isDirectory()) {
      await fsp.mkdir(destination, { recursive: true });
      for (const name of await fsp.readdir(source)) {
        if (excluded(name) || ['.bin', '.cache', '.modules.yaml', '.pnpm-workspace-state-v1.json'].includes(name)) continue;
        // Virtual packages are copied on demand through their dependency links.
        if (name === '.pnpm') continue;
        await copyTree(path.join(source, name), `${target}/${name}`);
      }
    } else if (stat.isFile()) {
      bytes += stat.size; if (bytes > LIMIT) fail('Selected plugins exceed package size limit');
      safeName(target); files.push({ source, destination });
    } else fail('Unsupported plugin dependency file');
  }
  async function copyConfig(source, target) {
    const stat = await fsp.lstat(source).catch(error => { if (error.code === 'ENOENT') return null; throw error; });
    if (!stat) return;
    if (!stat.isFile() || stat.isSymbolicLink() || stat.size > 1024 ** 2) fail('Configuration must be an ordinary bounded file');
    const text = await fsp.readFile(source, 'utf8');
    const value = configurationValue(text,job);
    const portable = portableConfiguration(value, contents.credentials, { HOME: home, ...(sourceSlot && contents.runtime ? { SLOT: sourceSlot } : {}) });
    await fsp.writeFile(path.join(stage, target), JSON.stringify(portable, null, 2)); configFiles.push(target);
  }
  for (const name of contents.profiles) {
    const source = path.join(home, 'profiles', name), target = `environment/profiles/${name}`;
    const stat = await fsp.lstat(source); if (!stat.isDirectory() || stat.isSymbolicLink()) fail('Selected profile must be an ordinary directory');
    await fsp.mkdir(path.join(stage, target), { recursive: true }); roots.push({ source: await fsp.realpath(source), target });
    const manifest = await json(path.join(source, 'package.json'), 1024 ** 2);
    if (!contents.plugins) {
      for (const key of ['dependencies', 'devDependencies', 'optionalDependencies', 'peerDependencies']) if (manifest[key]) manifest[key] = Object.fromEntries(Object.entries(manifest[key]).filter(([name]) => name.startsWith('@deepseek-ai/')));
      if (Array.isArray(manifest.dsh?.profile?.bundles)) manifest.dsh.profile.bundles = manifest.dsh.profile.bundles.filter(name => name.startsWith('@deepseek-ai/'));
    }
    await fsp.writeFile(path.join(stage, target, 'package.json'), JSON.stringify(manifest, null, 2));
    if (contents.configuration) await copyConfig(path.join(source, 'cordis.patch.yml'), `${target}/cordis.patch.yml`);
    else await fsp.writeFile(path.join(stage, target, 'cordis.patch.yml'), '{}');
    if (contents.configuration && contents.plugins) {
      const policy = path.join(home, 'profiles/.nexus-plugin-isolation', `${name}.json`);
      const value = await json(policy, 1024 ** 2).catch(error => { if (error.code === 'ENOENT') return null; throw error; });
      if (value) { await fsp.mkdir(path.join(environment, 'profiles/.nexus-plugin-isolation'), { recursive: true }); await fsp.writeFile(path.join(environment, 'profiles/.nexus-plugin-isolation', `${name}.json`), JSON.stringify(value)); }
    }
    const modules = path.join(source, 'node_modules');
    await fsp.mkdir(path.join(stage, target, 'node_modules'));
    if (!contents.plugins) continue;
    if (!(await fsp.stat(modules).catch(() => null))?.isDirectory()) { if (contents.plugins) fail(`Profile ${name} has no installed dependencies; complete its online setup first`); continue; }
    for (const name of await fsp.readdir(modules)) {
      if (name.startsWith('.')) continue;
      await copyTree(path.join(modules, name), `${target}/node_modules/${name}`);
    }
  }
  for (let index = 0; index < queue.length; index++) await copyTree(queue[index].source, queue[index].target);
  await ensureSpaceBudget([{ path: job.work, bytes, entries: count }]);
  let copied = 0; report('copy_environment', 0, files.length);
  await parallel(files, async ({ source, destination }) => { await fsp.copyFile(source, destination, fs.constants.COPYFILE_EXCL); report('copy_environment', ++copied, files.length); });
  if (contents.environment) {
    for (const name of ['settings.yaml', 'cordis.patch.yml']) await copyConfig(path.join(home, name), `environment/${name}`);
  }
  if (contents.credentials) for (const name of ['.env', '.credentials.yaml']) {
    const source = path.join(home, name), stat = await fsp.lstat(source).catch(error => { if (error.code === 'ENOENT') return null; throw error; });
    if (stat) { if (!stat.isFile() || stat.isSymbolicLink() || stat.size > 1024 ** 2) fail('Environment credentials file is not ordinary'); await fsp.copyFile(source, path.join(environment, name)); }
  }
  if (contents.sessions) for (const name of ['sessions', 'storages']) {
    const source = path.join(home, name);
    if (await fsp.lstat(source).catch(error => { if (error.code === 'ENOENT') return null; throw error; })) await copyOrdinaryTree(source, path.join(environment, name), job.work);
  }
  const preferences = contents.environment ? portableConfiguration(job.preferences || {}, contents.credentials, { HOME: home, ...(sourceSlot && contents.runtime ? { SLOT: sourceSlot } : {}) }) : {};
  delete preferences.home;
  // These point to separately managed files or downloads; do not silently emit
  // a supposedly complete environment with missing external assets.
  if (['patches', 'patch_entries', 'agents_home', 'bundled_skill_dir'].some(key => preferences[key]?.length)) fail('Export uses external patches or asset directories; move those files into the selected profile configuration before migration');
  return { contents, preferences, active_profile: contents.profiles.includes(job.active_profile) ? job.active_profile : contents.profiles[0], config_files: configFiles };
}

async function copyOrdinaryTree(source, destination, work, relocate = null) {
  const files = [], directories = [], links = []; let bytes = 0, count = 0;
  async function visit(from, to) {
    if (++count > COUNT) fail('Environment exceeds package entry limit');
    const stat = await fsp.lstat(from);
    if (stat.isSymbolicLink()) {
      if (!relocate) fail('Session data must not contain links to other directories');
      const target = await fsp.realpath(from), internal = within(source, target);
      links.push({ to, target: internal ? path.join(relocate, path.relative(source, target)) : target, directory: (await fsp.stat(from)).isDirectory() });
    } else if (stat.isDirectory()) {
      directories.push(to);
      for (const name of await fsp.readdir(from)) await visit(path.join(from, name), path.join(to, name));
    } else if (stat.isFile()) {
      bytes += stat.size; if (bytes > LIMIT) fail('Environment exceeds package size limit'); files.push({ from, to });
    } else fail('Unsupported environment file');
    report('copy_environment', count);
  }
  await visit(source, destination);
  await ensureSpaceBudget([{ path: work, bytes, entries: count }]);
  for (const directory of directories) await fsp.mkdir(directory, { recursive: true });
  let done = 0;
  await parallel(files, async ({ from, to }) => { await fsp.copyFile(from, to, fs.constants.COPYFILE_EXCL); report('copy_environment', ++done, files.length); });
  for (const link of links) await fsp.symlink(link.target, link.to, link.directory ? 'junction' : 'file');
}

async function mergeEnvironment(job) {
  const payload = path.join(job.work, 'payload'), incoming = path.join(payload, 'environment'), merged = path.join(payload, 'merged-environment');
  const manifest = await json(path.join(payload, 'manifest.json')); validateManifest(manifest);
  if (!manifest.entries.some(entry => entry.path === 'environment')) return;
  const preserveCredentials = !manifest.contents.credentials || manifest.contents.credential_policy !== 'replace';
  let preservedValues=0;
  const replaced = [];
  // A dependency stored at the same source path may have changed since a prior
  // export. Give incoming units their own names so other profiles keep theirs.
  const packages = path.join(incoming, '.packages'), units = await fsp.readdir(packages).catch(error => { if (error.code === 'ENOENT') return []; throw error; });
  const namespace = crypto.createHash('sha256').update(job.environment).digest('hex').slice(0, 16), renamed = new Map();
  for (const unit of units) {
    const name = `${namespace}-${unit}`;
    await fsp.rename(path.join(packages, unit), path.join(packages, name));
    renamed.set(`environment/.packages/${unit}`, `environment/.packages/${name}`);
  }
  if (renamed.size) {
    const remap = value => { const root = value.match(/^environment\/\.packages\/[^/]+/)?.[0]; return renamed.has(root) ? renamed.get(root) + value.slice(root.length) : value; };
    manifest.entries = manifest.entries.map(entry => ({ ...entry, path: remap(entry.path) }));
    manifest.links = manifest.links.map(link => ({ ...link, path: remap(link.path), target: remap(link.target) }));
    await fsp.writeFile(path.join(payload, 'manifest.json'), JSON.stringify(manifest));
  }
  const home = job.home && await fsp.lstat(job.home).catch(error => { if (error.code === 'ENOENT') return null; throw error; });
  if (home) {
    if (!home.isDirectory() || home.isSymbolicLink()) fail('Existing data directory is not ordinary');
    await copyOrdinaryTree(await fsp.realpath(job.home), merged, job.work, job.environment);
  } else await fsp.mkdir(merged);
  // Preserve unselected plugin installations and credentials in the receiver.
  const retained = new Map();
  async function configuration(file) {
    const stat = await fsp.lstat(file);
    if (!stat.isFile() || stat.isSymbolicLink() || stat.size > 1024 ** 2) fail('Existing configuration is not ordinary');
    const text = await fsp.readFile(file, 'utf8');
    return configurationValue(text,job);
  }
  if (manifest.contents.profiles.length) {
    const profiles = path.join(merged, 'profiles'), stat = await fsp.lstat(profiles).catch(error => { if (error.code === 'ENOENT') return null; throw error; });
    if (stat && (!stat.isDirectory() || stat.isSymbolicLink())) fail('Profile directory must not redirect outside the imported environment');
  }
  for (const name of manifest.contents.profiles) {
    if (!validProfile(name)) fail('Invalid profile overlay');
    const target = path.join(merged, 'profiles', name), stat = await fsp.lstat(target).catch(error => { if (error.code === 'ENOENT') return null; throw error; });
    if (stat && (!stat.isDirectory() || stat.isSymbolicLink())) fail('Existing profile must be an ordinary directory');
    const patch = path.join(target, 'cordis.patch.yml');
    if (preserveCredentials && await fsp.lstat(patch).catch(() => null)) retained.set(`profiles/${name}/cordis.patch.yml`, await configuration(patch));
    // Replace the plugin installation only; configuration is an independent component.
    if (manifest.contents.plugins) await fsp.rm(path.join(target, 'node_modules'), { recursive: true, force: true });
    else if (stat) {
      const file = path.join(incoming, 'profiles', name, 'package.json'), next = await json(file), previous = await configuration(path.join(target, 'package.json'));
      for (const key of ['dependencies', 'devDependencies', 'optionalDependencies', 'peerDependencies']) {
        if (Object.hasOwn(previous, key)) next[key] = previous[key]; else delete next[key];
      }
      if (next.dsh?.profile) {
        if (previous.dsh?.profile?.bundles) next.dsh.profile.bundles = previous.dsh.profile.bundles;
        else delete next.dsh.profile.bundles;
      }
      await fsp.writeFile(file, JSON.stringify(next, null, 2));
    }
  }
  async function overlay(relative) {
    const from = path.join(incoming, relative), to = path.join(merged, relative), stat = await fsp.lstat(from);
    if (stat.isDirectory()) {
      const existing = await fsp.lstat(to).catch(error => { if (error.code === 'ENOENT') return null; throw error; });
      if (existing && (!existing.isDirectory() || existing.isSymbolicLink())) fail('Imported directory conflicts with an existing file');
      await fsp.mkdir(to, { recursive: true });
      for (const name of await fsp.readdir(from)) await overlay(path.join(relative, name));
    } else if (stat.isFile()) {
      const existing = await fsp.lstat(to).catch(error => { if (error.code === 'ENOENT') return null; throw error; });
      if (existing && (!existing.isFile() || existing.isSymbolicLink())) fail('Imported file conflicts with an existing directory');
      if (/^(sessions|storages)(\/|$)/.test(slash(relative)) && existing && await hash(from) !== await hash(to)) fail('A session or attachment with the same identity has different content; the current environment was preserved');
      const credentialFile = ['.env', '.credentials.yaml'].includes(slash(relative));
      const configFile = (manifest.config_files || []).includes(`environment/${slash(relative)}`);
      if (existing && (credentialFile || configFile) && !preserveCredentials) replaced.push(slash(relative));
      if (credentialFile && existing && preserveCredentials) return;
      if (preserveCredentials && configFile && (existing || retained.has(slash(relative)))) {
        const previous = retained.has(slash(relative)) ? retained.get(slash(relative)) : await configuration(to);
        await fsp.writeFile(to, JSON.stringify(keepCredentials(await json(from), previous, () => { preservedValues++; }), null, 2));
      } else await fsp.copyFile(from, to);
    } else fail('Imported data must contain ordinary files');
  }
  await overlay('');
  if (replaced.length) await fsp.writeFile(path.join(job.work, 'credential-recovery.json'), JSON.stringify({
    schema_version: 1, operation_id: job.id, created_at_unix: Math.floor(Date.now() / 1000),
    previous_home: await fsp.realpath(job.home), imported_home: job.environment,
    files: replaced.sort(), instruction: 'Original credential files remain in previous_home. Stop Harness before restoring them.'
  }));
  // Paths in retained configuration also follow the new home. Do not rewrite
  // conversation text or credential files.
  if (home) {
    const retained = ['settings.yaml', 'cordis.patch.yml'];
    for (const name of await fsp.readdir(path.join(merged, 'profiles')).catch(() => [])) if (validProfile(name)) retained.push(`profiles/${name}/cordis.patch.yml`);
    for (const relative of retained) {
      const file = path.join(merged, relative), stat = await fsp.lstat(file).catch(() => null);
      if (!stat) continue;
      if (!stat.isFile() || stat.isSymbolicLink() || stat.size > 1024 ** 2) fail('Retained configuration is not ordinary');
      if (!within(merged, await fsp.realpath(file))) continue;
      const value = await configuration(file);
      const relocated = relocateConfiguration(value, job.home, job.environment);
      if (JSON.stringify(value) !== JSON.stringify(relocated)) await fsp.writeFile(file, JSON.stringify(relocated, null, 2));
    }
  }
  // Count only: never put credential values or user-defined key names in a
  // result notice. This is written only after the complete merge succeeds.
  await fsp.writeFile(path.join(job.work,'merge-summary.json'),JSON.stringify({schema_version:1,preserved_configuration_values:preservedValues}));
}

async function environmentShims(environment, slot, tools) {
  async function visit(directory) {
    const entries = await fsp.readdir(directory, { withFileTypes: true });
    if (path.basename(directory) === 'node_modules') {
      const packages = [];
      for (const entry of entries) {
        if (entry.name.startsWith('.')) continue;
        const file = path.join(directory, entry.name);
        if (entry.name.startsWith('@')) { for (const name of await fsp.readdir(file)) packages.push(path.join(file, name)); }
        else packages.push(file);
      }
      for (const folder of packages) {
        const manifest = await json(path.join(folder, 'package.json'), 1024 ** 2).catch(error => { if (error.code === 'ENOENT') return null; throw error; });
        if (!manifest?.bin) continue;
        const bins = typeof manifest.bin === 'string' ? { [manifest.name.split('/').at(-1)]: manifest.bin } : manifest.bin;
        for (const [name, relative] of Object.entries(bins)) {
          if (!/^[\w.-]+$/.test(name) || typeof relative !== 'string') fail('Invalid plugin executable');
          const file = path.resolve(folder, relative);
          if (!within(folder, file)) fail('Plugin executable leaves its package');
          const real = await fsp.realpath(file);
          if (!within(await fsp.realpath(environment), real) && !within(await fsp.realpath(slot), real)) fail('Plugin executable leaves environment');
          await fsp.mkdir(path.join(directory, '.bin'), { recursive: true }); await tools.shim(file, path.join(directory, '.bin', name));
        }
      }
    }
    for (const entry of entries) if (entry.isDirectory() && entry.name !== '.bin') await visit(path.join(directory, entry.name));
  }
  await visit(environment);
}

export async function desktopRuntimeForExport(slot, runtime, fallback) {
  const lock = ['scripts/primary-runtime/lock.json', 'apps/desktop/scripts/primary-runtime-lock.json']
    .map(file => path.join(slot, file)).find(file => fs.existsSync(file));
  if (!lock) return null; // Older Web-only releases remain portable.
  const targetName = `${({ win32: 'win', darwin: 'mac', linux: 'linux' })[process.platform]}-${process.arch}`;
  const buildPaths = path.join(slot, 'apps/desktop/scripts/desktop-build-paths.mjs');
  if (fs.existsSync(buildPaths)) {
    const declaration = /SUPPORTED_TARGETS\s*=\s*new Set\(\[([^\]]+)\]\)/.exec(await fsp.readFile(buildPaths, 'utf8'))?.[1];
    if (!declaration || ![...declaration.matchAll(/['"]([^'"]+)['"]/g)].some(match => match[1] === targetName)) return null;
  } else if (process.platform === 'linux') return null;
  const spec = await json(lock), target = spec.targets[`${({ win32: 'win', darwin: 'mac', linux: 'linux' })[process.platform]}-${process.arch}`];
  if (!target) return null; // Upstream has no Desktop for this platform; preserve Web portability.
  const app = path.join(slot, 'apps/desktop');
  const electron = await json(path.join(app, 'node_modules/electron/package.json'));
  const pnpm = await json(path.join(app, 'node_modules/pnpm/package.json'));
  for (const directory of [path.join(runtime, 'desktop'), fallback].filter(Boolean)) {
    if (!fs.existsSync(path.join(directory, 'manifest.json'))) continue;
    const manifest = await json(path.join(directory, 'manifest.json'));
    const bundledLock = path.join(directory, 'lock.json');
    const { targets: sourceTargets, ...sourceCommon } = spec;
    const { targets: bundledTargets, ...bundledCommon } = await json(bundledLock);
    if (!isDeepStrictEqual(sourceCommon, bundledCommon) || !isDeepStrictEqual(sourceTargets[targetName], bundledTargets?.[targetName]) ||
        manifest.electronVersion !== electron.version || manifest.pnpmVersion !== pnpm.version) continue;
    const expected = manifest.lockSha256;
    if (![1, 2, 3].includes(manifest.schema) || manifest.platform !== process.platform || manifest.arch !== process.arch || !Array.isArray(manifest.files)) fail('Invalid Desktop offline runtime');
    const names = new Set();
    for (const entry of manifest.files) {
      safeName(`runtime/desktop/${entry.path}`);
      if (names.has(entry.path)) fail('Duplicate Desktop offline runtime entry');
      names.add(entry.path);
      const file = path.join(directory, entry.path), stat = await fsp.lstat(file);
      if (!stat.isFile() || stat.isSymbolicLink() || await hash(file) !== entry.sha256) fail('Desktop offline runtime integrity check failed');
    }
    if (manifest.schema === 3) {
      if (manifest.supported !== true || manifest.electronMode !== 'launcher' || manifest.primarySmokePassed !== true ||
          !names.has('primary.tar.gz') || !names.has('lock.json') || await hash(path.join(directory, 'lock.json')) !== expected ||
          await hash(path.join(directory, 'primary.tar.gz')) !== manifest.primaryArchiveSha256) fail('Incomplete shared Desktop offline runtime');
      if (manifest.hostArchiveSha256 && (!names.has('host.tar.gz') || await hash(path.join(directory, 'host.tar.gz')) !== manifest.hostArchiveSha256)) fail('Incomplete portable Electron host');
      return directory; // Electron is supplied by the compatible Nexus installation.
    }
    const electronFile = manifest.schema === 2 ? 'electron.zip' : process.platform === 'win32' ? 'electron/electron.exe' : 'electron/Electron.app/Contents/MacOS/Electron';
    if (!names.has(electronFile) || !names.has('lock.json') || await hash(path.join(directory, 'lock.json')) !== expected) fail('Incomplete Desktop offline runtime');
    if (manifest.schema === 2 && (manifest.supported !== true || await hash(path.join(directory, electronFile)) !== manifest.electronArchiveSha256)) fail('Invalid Desktop offline archive');
    for (const hash of [target.nodeSha256, target.pythonSha256, ...target.wheels.map(item => item.sha256), ...spec.wheels.map(item => item.sha256)]) {
      if (!names.has(`assets/${hash}`)) fail('Incomplete Desktop offline assets; repair Nexus before exporting');
    }
    return directory;
  }
  fail('No matching Desktop offline runtime. Update or repair Nexus before exporting this Harness version.');
}

// Exports remain self-contained even if the receiving Nexus uses a different
// Electron. Package only the immutable application host, never its data root.
export async function desktopHostForExport(desktop, job) {
  if (!desktop) return null;
  const manifest = await json(path.join(desktop, 'manifest.json'));
  if (manifest.schema !== 3 || manifest.hostArchiveSha256) return null;
  if (!['win32', 'darwin'].includes(process.platform)) fail('Official Desktop host is unsupported on this platform');
  const root = await fsp.realpath(job.desktop_host || path.join(path.dirname(job.private_writer), windows ? '..' : '../../..'));
  const resources = windows ? 'resources' : 'Nexus Launcher.app/Contents/Resources';
  const host = await json(path.join(root, resources, 'nexus-electron-host.json'));
  if (host.schema !== 1 || host.entry !== 'nexus-official-desktop' || host.electronVersion !== manifest.electronVersion ||
      host.appAsarSha256 !== await hash(path.join(root, resources, 'app.asar'))) fail('Export requires a verified Nexus host with official Desktop support');
  const executable = path.join(root, windows ? 'Nexus Launcher.exe' : 'Nexus Launcher.app/Contents/MacOS/Nexus Launcher');
  const probe = spawnSync(executable, ['-p', 'process.versions.electron'], {
    encoding: 'utf8', windowsHide: true, timeout: 15000, env: { ...process.env, ELECTRON_RUN_AS_NODE: '1' },
  });
  if (probe.status !== 0 || probe.stdout.trim() !== manifest.electronVersion) fail('Export needs the matching Nexus Electron host or an existing complete offline package');
  const names = windows ? ['Nexus Launcher.exe', 'chrome_100_percent.pak', 'chrome_200_percent.pak', 'd3dcompiler_47.dll', 'dxcompiler.dll', 'dxil.dll', 'ffmpeg.dll', 'icudtl.dat',
    'resources.pak', 'snapshot_blob.bin', 'v8_context_snapshot.bin', 'vk_swiftshader.dll', 'vk_swiftshader_icd.json', 'vulkan-1.dll',
    'LICENSE.electron.txt', 'LICENSES.chromium.html', 'locales', 'resources/app.asar', 'resources/app.asar.unpacked', 'resources/nexus-electron-host.json'] : ['Nexus Launcher.app'];
  let bytes = 0, entries = 0;
  async function measure(file) {
    if (++entries > COUNT) fail('Portable Electron host has too many entries');
    const stat = await fsp.lstat(file);
    if (stat.isSymbolicLink()) {
      const raw = await fsp.readlink(file), resolved = await fsp.realpath(file);
      if (windows || path.isAbsolute(raw) || !within(path.join(root, 'Nexus Launcher.app'), resolved)) fail('Portable Electron host link escapes its app');
      return;
    }
    if (stat.isDirectory()) for (const name of await fsp.readdir(file)) await measure(path.join(file, name));
    else if (stat.isFile()) bytes += stat.size;
    else fail('Invalid portable Electron host');
  }
  if (!windows) {
    const verify = spawnSync('/usr/bin/codesign', ['--verify', '--deep', '--strict', path.join(root, 'Nexus Launcher.app')], { encoding: 'utf8', timeout: 30000 });
    if (verify.status !== 0) fail('Cannot export an invalid macOS app signature');
  }
  for (const name of names) await measure(path.join(root, name));
  return { root, names, bytes, entries };
}

async function pack(job, tools) {
  const base = job.contents?.runtime !== false;
  const desktop = base ? await desktopRuntimeForExport(job.slot, job.runtime, job.desktop_runtime) : null;
  const host = await desktopHostForExport(desktop, job);
  const extraDesktop = desktop && path.resolve(desktop) !== path.resolve(job.runtime, 'desktop') ? desktop : null;
  job.contents = { profiles: [], configuration: false, plugins: false, credentials: false, ...job.contents, runtime: base, environment: job.contents?.environment ?? job.contents?.configuration ?? false, sessions: job.contents?.sessions ?? false };
  const sourceSlot = base ? await measureSource(job.slot, 'slot') : { bytes: 0, entries: 0 }, sourceRuntime = base ? await measureSource(job.runtime, 'runtime') : { bytes: 0, entries: 0 };
  const desktopSize = extraDesktop ? await measureSource(extraDesktop, 'runtime') : { bytes: 0, entries: 0 };
  // Always carry the distributable Git toolchain, including when exporting
  // an older imported runtime that predates bundled Git. Explicit host paths
  // are machine-local and are not copied into a portable archive.
  const gitSource = base ? job.git_runtime : null;
  if (base && (!gitSource || !fs.existsSync(path.join(gitSource, windows ? 'cmd/git.exe' : 'bin/git')))) fail('Complete bundled Git is required for offline export');
  const gitSize = gitSource ? await measureSource(gitSource, 'runtime') : { bytes: 0, entries: 0 };
  const sourceEntries = sourceSlot.entries + sourceRuntime.entries + desktopSize.entries + gitSize.entries;
  // Reserve bounded manifest plus shim/path normalization growth. The archive
  // budget allows tar headers, long paths and incompressible gzip overhead.
  const stageBytes = sourceSlot.bytes + sourceRuntime.bytes + desktopSize.bytes + gitSize.bytes + (host ? archiveBudget(host.bytes, host.entries) : 0) + sourceEntries * 8192 + MANIFEST_LIMIT;
  await ensureSpaceBudget([
    { path: job.work, bytes: stageBytes, entries: sourceEntries + 1 },
    { path: path.dirname(job.archive), bytes: archiveBudget(stageBytes, sourceEntries), entries: 1 },
  ]);
  const stage = path.join(job.work, 'payload'); await fsp.mkdir(stage);
  const slot = path.join(stage, 'slot'), runtime = path.join(stage, 'runtime'), links = [];
  process.stderr.write('Offline: copying slot and runtime\n');
  if (base) { await walkCopy(job.slot, slot, 'slot', links); await walkCopy(job.runtime, runtime, 'runtime', links); }
  if (gitSource) {
    const target = path.join(runtime, 'git');
    await fsp.rm(target, { recursive: true, force: true });
    await walkCopy(gitSource, target, 'runtime/git', links);
  }
  if (extraDesktop) {
    const target = path.resolve(runtime, 'desktop');
    if (!within(stage, target)) fail('Invalid Desktop staging directory');
    await fsp.rm(target, { recursive: true, force: true });
    await walkCopy(extraDesktop, target, 'runtime/desktop', links);
  }
  if (host) {
    const directory = path.join(runtime, 'desktop'), archive = path.join(directory, 'host.tar.gz');
    await tools.tar.c({ cwd: host.root, file: archive, gzip: { level: 1 }, portable: true, noMtime: true, strict: true }, host.names);
    const manifest = await json(path.join(directory, 'manifest.json'));
    manifest.hostArchiveSha256 = await hash(archive);
    manifest.files = manifest.files.filter(entry => entry.path !== 'host.tar.gz');
    manifest.files.push({ path: 'host.tar.gz', sha256: manifest.hostArchiveSha256 });
    await fsp.writeFile(path.join(directory, 'manifest.json'), JSON.stringify(manifest, null, 2));
  }
  const migration = await captureEnvironment(job, stage, links);
  process.stderr.write(`Offline: normalizing ${links.length} internal links and executable shims\n`);
  await materialize(links, slot, runtime); if (base) await normalize(slot, tools);
  if (migration) await environmentShims(path.join(stage, 'environment'), slot, tools);
  const versions = base ? await runtimeIdentity(runtime) : null;
  report('remove_links', 0, links.length);
  let removed = 0;
  for (const link of links.reverse()) { await fsp.unlink(componentPath(link.path, slot, runtime)); report('remove_links', ++removed, links.length); } links.reverse();
  process.stderr.write('Offline: hashing normalized files\n');
  const files = await inventory(stage, !!migration, base);
  const manifest = { schema: 3, platform: process.platform, arch: process.arch, nexus: job.nexus, version: job.version, versions, links, ...files, contents: job.contents, ...migration };
  validateManifest(manifest);
  const text = JSON.stringify(manifest); if (Buffer.byteLength(text) > MANIFEST_LIMIT) fail('Manifest is too large');
  await fsp.writeFile(path.join(stage, 'manifest.json'), text);
  // Copy/normalization has finished: only the archive remains to be allocated.
  await ensureSpaceBudget([{ path: path.dirname(job.archive), bytes: archiveBudget(files.total + Buffer.byteLength(text), files.entries.length), entries: 1 }]);
  const temp = `${job.archive}.nexus-${job.id}.tmp`;
  let privateCreated = false;
  try {
    try { await fsp.lstat(job.archive); fail('Export destination already exists'); } catch (error) { if (error.code !== 'ENOENT') throw error; }
    process.stderr.write(`Offline: writing archive (${files.entries.length} entries)\n`);
    let archived = 0; report('compress', 0, null, 'bytes');
    const counter = new Transform({ transform(chunk, encoding, callback) {
      archived += chunk.length; report('compress', archived, null, 'bytes'); callback(null, chunk);
    } });
    const archiveStream = tools.tar.c({ cwd: stage, gzip: { level: 1 }, portable: true, noMtime: true, strict: true },
      ['manifest.json', ...(base ? ['slot', 'runtime'] : []), ...(migration ? ['environment'] : [])]);
    if (!job.private_writer) fail('Private archive writer is required');
    const writer = spawn(job.private_writer, ['--write-private-archive', temp], { windowsHide: true, stdio: ['pipe', 'ignore', 'pipe'] });
    let errorText = '';
    writer.stderr.on('data', chunk => { errorText = (errorText + chunk.toString()).slice(-4096); });
    const completed = new Promise((resolve, reject) => {
      writer.once('error', reject);
      writer.once('close', code => { if (code === 0) { privateCreated = true; resolve(); } else reject(new Error(`Private archive writer failed: ${errorText}`)); });
    });
    // Both promises are observed immediately, including failure to spawn.
    const results = await Promise.allSettled([pipeline(archiveStream, counter, writer.stdin), completed]);
    for (const result of results) if (result.status === 'rejected') throw result.reason;
    report('flush_archive');
    await fsp.link(temp, job.archive);
  } finally { if (privateCreated) await fsp.unlink(temp).catch(() => {}); }
  await fsp.writeFile(path.join(job.work, 'result.json'), JSON.stringify({ version: manifest.version, versions, archive: job.archive }));
}
async function unpack(job, tools) {
  report('scan_archive');
  const stage = path.join(job.work, 'payload'); await fsp.mkdir(stage);
  const manifestChunks = []; let bytes = 0, count = 0; const seen = new Set();
  await tools.tar.t({ file: job.archive, strict: true, onentry(entry) {
    const name = safeName(entry.path.replace(/\/$/, ''));
    if (!['File', 'Directory'].includes(entry.type) || seen.has(name.toLowerCase()) || ++count > COUNT) fail('Unsupported or duplicate tar entry');
    seen.add(name.toLowerCase()); bytes += entry.size; if (bytes > LIMIT + MANIFEST_LIMIT) fail('Tar size budget exceeded');
    report('scan_archive', count);
    if (name === 'manifest.json') { if (entry.type !== 'File' || entry.size > MANIFEST_LIMIT) fail('Invalid tar manifest'); entry.on('data', chunk => { manifestChunks.push(chunk); }); }
  } });
  const manifest = JSON.parse(Buffer.concat(manifestChunks).toString('utf8')), expected = validateManifest(manifest); const extracted = new Set();
  // Manifest was scanned without extracting. Import allocates one payload;
  // subsequent runtime/slot publication is rename-only, not another copy.
  await ensureSpaceBudget([{ path: job.work, bytes: manifest.total + MANIFEST_LIMIT + 1024 ** 2, entries: manifest.entries.length + manifest.links.length + 2 }]);
  report('extract', 0, manifest.entries.length + 1);
  await tools.tar.x({ file: job.archive, cwd: stage, strict: true, preservePaths: false, noChmod: windows, filter(name, entry) {
    name = safeName(name.replace(/\/$/, '')); const lower = name.toLowerCase();
    if (extracted.has(lower) || !['File', 'Directory'].includes(entry.type)) fail('Tar changed during extraction'); extracted.add(lower);
    report('extract', extracted.size, manifest.entries.length + 1);
    if (name === 'manifest.json') return entry.type === 'File' && entry.size <= MANIFEST_LIMIT;
    const item = expected.get(lower);
    if (!item || item.path !== name || (item.kind === 'file' ? entry.type !== 'File' || entry.size !== item.size : entry.type !== 'Directory')) fail('Tar entry differs from manifest');
    if (!windows) entry.mode = item.kind === 'file' ? item.mode : 0o755;
    return true;
  } });
  if (JSON.stringify(await json(path.join(stage, 'manifest.json'))) !== JSON.stringify(manifest)) fail('Tar manifest changed during extraction');
  // File creation respects the receiver's umask. Restore only validated rwx
  // bits on verified ordinary paths, before comparing the portable inventory.
  if (!windows) await parallel(manifest.entries.filter(entry => entry.kind === 'file'), async entry => {
    const file = path.join(stage, entry.path);
    if (!(await fsp.lstat(file)).isFile()) fail('Imported executable is not an ordinary file');
    await fsp.chmod(file, entry.mode);
  });
  const base = manifest.contents?.runtime !== false;
  const actual = await inventory(stage, expected.has('environment'), base);
  if (JSON.stringify(actual.entries) !== JSON.stringify(manifest.entries) || actual.total !== manifest.total) fail('Offline file integrity check failed');
  if (base) for (const required of ['slot/apps/cli/lib/bin.js', `runtime/${nodeEntry}`, `runtime/${npmEntry}`, 'runtime/node/node_modules/npm/bin/npm-cli.js', 'runtime/pnpm/bin/pnpm.cjs']) if (expected.get(required)?.kind !== 'file') fail('Offline package is incomplete');
  // The archive cannot opt the receiver into credentials, even for older clients.
  const selected = job.contents ? await selectImportContents(stage, manifest, job.contents)
    : manifest.schema >= 2 ? await selectImportContents(stage, manifest, { runtime: true, environment: manifest.contents.configuration, sessions: false, ...manifest.contents, credentials: false, credential_policy: 'preserve' }) : manifest;
  await fsp.writeFile(path.join(job.work, 'result.json'), JSON.stringify({ version: selected.version, versions: selected.versions, archive: job.archive, contents: selected.contents, active_profile: selected.active_profile, preferences: selected.preferences }));
}

async function selectImportContents(stage, manifest, requested) {
  if (requested.credential_policy != null && !['preserve', 'replace'].includes(requested.credential_policy)) fail('Unsupported credential conflict policy');
  const available = { runtime: true, profiles: [], configuration: false, environment: false, sessions: false, plugins: false, credentials: false, ...manifest.contents };
  available.environment = manifest.contents?.environment ?? manifest.contents?.configuration ?? false;
  const selected = { ...requested, environment: requested.environment ?? false };
  if (!Array.isArray(selected.profiles) || !selected.profiles.every(name => available.profiles.includes(name))
    || new Set(selected.profiles).size !== selected.profiles.length
    || !['runtime', 'configuration', 'environment', 'sessions', 'plugins', 'credentials'].every(key => typeof selected[key] === 'boolean' && (!selected[key] || available[key]))
    || (!selected.runtime && !selected.profiles.length && !selected.environment && !selected.sessions && !selected.credentials)) fail('Selected import contents are not available in this package');
  const packageRoots = new Set();
  function selectedPath(name) {
    if (name === 'slot' || name.startsWith('slot/') || name === 'runtime' || name.startsWith('runtime/')) return selected.runtime;
    if (name === 'environment') return selected.profiles.length || selected.environment || selected.sessions || selected.credentials;
    if (name === 'environment/profiles') return selected.profiles.length;
    if (/^environment\/(sessions|storages)(\/|$)/.test(name)) return selected.sessions;
    if (['environment/.env', 'environment/.credentials.yaml'].includes(name)) return selected.credentials;
    if (['environment/settings.yaml', 'environment/cordis.patch.yml'].includes(name)) return selected.environment;
    for (const profile of selected.profiles) {
      const prefix = `environment/profiles/${profile}`;
      if (name === prefix || name.startsWith(prefix + '/')) {
        if (name.startsWith(prefix + '/node_modules')) return selected.plugins;
        if (name === prefix + '/cordis.patch.yml') return selected.configuration;
        return true;
      }
      if (name === `environment/profiles/.nexus-plugin-isolation/${profile}.json`) return selected.plugins && selected.configuration;
    }
    if (name === 'environment/profiles/.nexus-plugin-isolation') return selected.plugins && selected.configuration && selected.profiles.length;
    if (name === 'environment/.packages') return packageRoots.size > 0;
    return [...packageRoots].some(root => name === root || name.startsWith(root + '/'));
  }
  // Include only dependency units reachable from the selected profiles.
  let changed = true;
  while (changed) {
    changed = false;
    for (const link of manifest.links) if (selectedPath(link.path)) {
      const root = link.target.match(/^environment\/\.packages\/[^/]+/)?.[0];
      if (root && !packageRoots.has(root)) { packageRoots.add(root); changed = true; }
    }
  }
  const entries = manifest.entries.filter(entry => selectedPath(entry.path));
  const links = manifest.links.filter(link => selectedPath(link.path));
  const kept = new Set(entries.map(entry => entry.path));
  for (const entry of [...manifest.entries].reverse()) if (!kept.has(entry.path)) {
    const file = path.join(stage, entry.path);
    if (!within(stage, file)) fail('Import selection leaves staging directory');
    if (entry.kind === 'directory') await fsp.rmdir(file); else await fsp.unlink(file);
  }
  const configFiles = (manifest.config_files || []).filter(name => kept.has(name));
  if (!selected.plugins) for (const profile of selected.profiles) {
    const name = `environment/profiles/${profile}/package.json`, file = path.join(stage, name), value = await json(file);
    for (const key of ['dependencies', 'devDependencies', 'optionalDependencies', 'peerDependencies']) if (value[key]) value[key] = Object.fromEntries(Object.entries(value[key]).filter(([name]) => name.startsWith('@deepseek-ai/')));
    if (Array.isArray(value.dsh?.profile?.bundles)) value.dsh.profile.bundles = value.dsh.profile.bundles.filter(name => name.startsWith('@deepseek-ai/'));
    await fsp.writeFile(file, JSON.stringify(value, null, 2));
    const entry = entries.find(entry => entry.path === name); entry.size = (await fsp.stat(file)).size; entry.sha256 = await hash(file);
  }
  if (!selected.credentials) for (const name of configFiles) {
    const file = path.join(stage, name);
    await fsp.writeFile(file, JSON.stringify(portableConfiguration(await json(file, 1024 ** 2), false), null, 2));
    const entry = entries.find(entry => entry.path === name); entry.size = (await fsp.stat(file)).size; entry.sha256 = await hash(file);
  }
  const result = { ...manifest, schema: 3, contents: selected, entries, links, config_files: configFiles,
    total: entries.reduce((sum, entry) => sum + (entry.size || 0), 0), preferences: selected.environment ? portableConfiguration(manifest.preferences || {}, selected.credentials) : {},
    active_profile: selected.profiles.includes(manifest.active_profile) ? manifest.active_profile : selected.profiles[0], versions: selected.runtime ? manifest.versions : null };
  validateManifest(result);
  await fsp.writeFile(path.join(stage, 'manifest.json'), JSON.stringify(result));
  return result;
}
async function inspect(job, tools) {
  const chunks = []; let found = false, count = 0, bytes = 0;
  await tools.tar.t({ file: job.archive, strict: true, onentry(entry) {
    safeName(entry.path.replace(/\/$/, '')); bytes += entry.size;
    if (++count > COUNT || bytes > LIMIT + MANIFEST_LIMIT || !['File', 'Directory'].includes(entry.type)) fail('Package preview exceeds supported limits');
    if (entry.path === 'manifest.json') {
      if (entry.type !== 'File' || entry.size > MANIFEST_LIMIT || found) fail('Invalid package manifest'); found = true;
      entry.on('data', chunk => chunks.push(chunk));
    }
  } });
  const manifest = JSON.parse(Buffer.concat(chunks).toString('utf8')); validateManifest(manifest);
  await fsp.writeFile(path.join(job.work, 'result.json'), JSON.stringify({ version: manifest.version, versions: manifest.versions,
    contents: manifest.contents || null, active_profile: manifest.active_profile || null, files: manifest.entries.length, bytes: manifest.total }));
}
async function main() {
  if (!['win32', 'darwin', 'linux'].includes(process.platform) || !['x64', 'arm64'].includes(process.arch)) fail('Offline packages require a supported 64-bit desktop platform');
  const job = await json(process.argv[2], 65536); const tools = modules(job.tools);
  progressFile = path.join(job.work, 'progress.json'); progressId = job.id;
  report(job.action === 'finalize' ? 'publish' : 'prepare');
  if (!path.isAbsolute(job.work) || !path.isAbsolute(job.archive)) fail('Offline paths must be absolute');
  if (job.action === 'export') await pack(job, tools);
  else if (job.action === 'inspect') await inspect(job, tools);
  else if (job.action === 'import') await unpack(job, tools);
  else if (job.action === 'merge_environment') await mergeEnvironment(job);
  else if (job.action === 'finalize') {
    const manifest = await json(path.join(job.work, 'payload/manifest.json')); validateManifest(manifest);
    const environment = job.environment || path.join(job.runtime, 'environment');
    await materialize(manifest.links, job.slot, job.runtime, environment);
    if (manifest.schema >= 2 && manifest.entries.some(entry => entry.path === 'environment')) {
      for (const name of manifest.config_files || []) {
        safeName(name); if (!name.startsWith('environment/') || !manifest.entries.some(entry => entry.path === name && entry.kind === 'file')) fail('Invalid imported configuration path');
        const file = componentPath(name, job.slot, job.runtime, environment);
        const restore = value => Array.isArray(value) ? value.map(restore) : value && typeof value === 'object' ? Object.fromEntries(Object.entries(value).map(([key, item]) => [key, restore(item)])) : typeof value === 'string' ? value.replaceAll('__NEXUS_OFFLINE_HOME__', slash(environment)).replaceAll('__NEXUS_OFFLINE_SLOT__', slash(job.slot)) : value;
        await fsp.writeFile(file, JSON.stringify(restore(await json(file, 1024 ** 2)), null, 2));
      }
      await environmentShims(environment, job.slot, tools);
    }
    if (manifest.contents?.runtime !== false) {
      const observed = await runtimeIdentity(job.runtime);
      if (JSON.stringify(observed) !== JSON.stringify(manifest.versions)) fail('Offline runtime versions differ from manifest');
    }
  } else fail('Unknown offline operation');
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main().catch(error => { process.stderr.write(`Offline package: ${error.message}\n`); process.exitCode = 1; });
